# omnidroid Dynamic RE Workbench — Design

Date: 2026-10-01
Status: draft (awaiting owner review)

## Goal

Let an LLM agent, working only through omnidroid's MCP, take an arbitrary arm64
Android `.so`, understand it by dynamically exercising it, write pseudo-C for it,
build that C back into an arm64 `.so`, and **prove** the rebuilt library is
behaviorally equivalent to the original. Exact source recovery is impossible and
is not attempted; the deliverable is behavior-matching pseudo-source verified by
differential testing.

The loop the agent runs, entirely over MCP:

```
APK ─▶ load target + deps (imports resolved) ─▶ generate input corpus
     ─▶ trace/observe behavior ─▶ [agent writes C] ─▶ build C → candidate.so
     ─▶ diff original vs candidate over the corpus ─▶ verdict + first divergence
     └────────────────────────── iterate until match ──────────────────────────┘
```

## Scope

**In scope (this spec):** the *isolated* RE→rebuild→verify loop — crafted and
fuzzed inputs, no live game. This is the clean, high-value 90% path: pure and
near-pure functions (crypto, checksums, decoders, deobfuscators, serializers,
license/integrity checks).

**Out of scope (this spec):**

- Live-game attachment, `frida-server`, guest `ptrace` — explicitly dropped.
  (The real goal is understanding/reproducing behavior, not hooking a live app.)
- Exact source recovery (impossible; behavioral match only).
- A decompiler UI — the LLM agent *is* the decompiler; omnidroid supplies the
  dynamic evidence and the equivalence oracle.
- Functions whose behavior depends on live global state or on bionic imports the
  HLE layer does not model — deferred to Spec 2 (live capture).

## Current state (anchors in the tree)

- `omni_debug::Session` (`crates/omni-debug/src/session.rs`): a single-module lab
  session. `Session::load` hardcodes `ProviderRegistry::empty_provider()`
  (`session.rs:316`), so **imports are never resolved** — any call into
  `libc`/`libm`/`liblog`/a sibling `.so` faults at `0x0`. This is the #1 blocker
  for real-world libraries.
- `omni_elf::loader` already has the full resolution machinery:
  `ProviderRegistry`, `SymbolProvider`, `SymbolRequest`, `Binding`, `EmptyProvider`
  (`crates/omni-elf/src/loader/mod.rs`). The lab simply opts out of it.
- `omni_android::boundary::BoundaryBuilder` **implements `SymbolProvider`**
  ("omnidroid-thunks", `crates/omni-android/src/boundary.rs:513`) — it thunks
  bionic functions to omnidroid's host implementations and reports unmodeled ones
  by name. This is the proven provider the engine-alone path uses.
- `sysroot/aosp-35` holds real AOSP-35 objects (content-addressed: `objects/` +
  `sysroot.manifest`), including libc/libm, available to co-load when HLE is
  insufficient.
- `omni-mcp` `Server` holds exactly one `lab: Option<Session>`
  (`crates/omni-mcp/src/server.rs:116`); every lab tool routes through
  `lab_mut()` (`server.rs:475`). Tools are registered in the static `TOOLS` array
  (`server.rs:~1057`) + a `call_tool` match arm + a handler; input schemas are
  auto-generated.
- Proven lab primitives already exposed over MCP: `lab_load`, `resolve_symbol`,
  `list_symbols`, `list_maps`, `read_mem`, `write_mem`, `alloc_data`, `load_code`,
  `call_function`, `set_breakpoint`/`run_until_stop`/`resume`, `intercept`,
  `trace_syscalls`, `dump_module`, `get_registers`, `backtrace`.

## Design — four capabilities

### C1. Dependency-resolving load (fixes "imports fault at 0x0")

Add a load path that builds a `ProviderRegistry` instead of using
`EmptyProvider`, chaining, in priority order:

1. modules already loaded in the session (so co-loaded sibling `.so` from the same
   APK resolve each other's symbols);
2. `omni_android::boundary::BoundaryBuilder` as the bionic provider, so
   `libc`/`libm`/`liblog`/`libdl` calls run against omnidroid's host
   implementations;
3. (optional, when HLE is insufficient) real system `.so` co-loaded from
   `sysroot/aosp-35`.

API: a `Session::load_resolved(name, path)` (or a `LoadOptions`/builder on `load`)
that assembles the registry from the above. The existing `Session::load` (empty
provider) is kept for the fully self-contained case.

New MCP tool `lab_load_apk {apk, target, with_deps?, resolver?}`:

- extracts `lib/arm64-v8a/*.so` from the APK;
- loads `target` together with the sibling libs it `DT_NEEDED`s and that are
  present in the APK, under the resolving provider;
- returns: loaded modules, `target` exports, and the **residual unresolved
  imports** (from `omni_elf::loader::Imports::unresolved`) so the agent knows
  exactly what is still stubbed and must be `intercept`ed or modeled.

`lab_load` continues to work unchanged for a single self-contained `.so`.

### C2. Deep observation (so the agent can infer logic, not just I/O)

Extend the run loop to record, for a single `call_function`, a structured
execution trace built on the existing breakpoint/step machinery, `TraceEvent`,
and `GuestSpace` access:

- branch decisions (basic-block edges: from-addr, to-addr, taken);
- memory reads/writes (addr, len, value) within a configurable budget;
- call/ret edges (caller, callee, return value);
- syscalls (reusing `trace_syscalls`).

**Arg model (`arg_spec`).** Each parameter is declared as one of: `scalar(u64)`,
`in_buffer(hex | len+fill)`, `out_buffer(len)`, `ptr`. The harness allocates
buffers with `alloc_data`, passes their addresses in X0..X7, and reads
`out_buffer`s back after the call. This makes buffer-taking functions first-class
for both tracing and diffing.

New MCP tool `lab_trace {target, arg_spec, args, record:[branches|mem|calls|syscalls], max_events}`
→ `CallOutcome` (ret/ret1, out-buffer contents) + the requested trace, with the
existing 200M-instruction budget and explicit truncation reporting.

### C3. Build loop (candidate C → arm64 `.so`)

A host-side build helper (new module in `omni-mcp`, or a thin new crate
`omni-rebuild`) that invokes an arm64 clang to compile agent-supplied C into a PIC
`.so` loadable by the same resolving provider as C1:

- toolchain: Android NDK clang, or plain LLVM with
  `--target=aarch64-linux-android` (fall back to a freestanding
  `aarch64-none-elf` target for self-contained code);
- the candidate is built `-shared -fPIC`, exporting the function(s) under test;
  its own imports are resolved by the C1 provider when loaded.

New MCP tool `lab_build {sources:[{name, text}], flags?, soname?}` → the output
`.so` path, or **structured compiler diagnostics** (so the agent can fix its C).

**Prerequisite (flagged, detected at runtime):** an arm64 clang toolchain must be
on the host. The tool detects it (env `OMNI_NDK` / `ANDROID_NDK_HOME`, or a clang
on `PATH` that accepts the aarch64 target) and, when absent, returns a clear
"toolchain missing" error naming the expected env — it never silently produces a
wrong-arch artifact.

### C4. Differential oracle (the equivalence proof)

A `DiffHarness` (in `omni-debug`, driven from `omni-mcp`) holding two sessions —
`original` and `candidate` — loaded under **identical** providers so that any
difference is attributable to the code, not the environment.

- `lab_corpus {target, arg_spec, seeds?, count}` — generates input vectors:
  explicit `seeds` plus deterministic seeded mutation/fuzzing shaped by the
  `arg_spec` (boundary values, lengths, random fills from a fixed PRNG seed).
  Stored and referenced by a corpus id; regenerating from the same seed is
  reproducible.
- `lab_diff {original, candidate, target, corpus, compare:[ret, out_buffers, mem_writes, syscalls], ignore?}`
  — runs both libraries over every input in the corpus and compares the selected
  observables. Returns: a **match ratio**, the **first divergence** (the input,
  the expected vs got value, and which observable diverged), and summary stats.
  `ignore` lets the agent exclude observables known to be nondeterministic.

The agent iterates C2 → C3 → C4 until the match ratio reaches a threshold it
sets. **Equivalence is explicitly relative to the `arg_spec` and corpus** — the
oracle proves "matches on everything tested," not universal equivalence; the spec
and tool output state this limitation plainly.

### Server state refactor

The `Server`'s single `lab: Option<Session>` becomes a small named-session
registry (keys include `default`, `original`, `candidate`, and co-loaded deps).
`lab_mut()` is refactored to select by name, defaulting to `default` so that every
existing single-lab tool keeps working with no behavior change.

## MCP tool surface (new)

| tool | purpose |
|---|---|
| `lab_load_apk` | extract + load target with deps under the resolving provider; report residual unresolved imports |
| `lab_trace` | call with an `arg_spec`, return outputs + a structured behavior trace |
| `lab_corpus` | generate/record a reproducible input corpus for a target |
| `lab_build` | cross-compile candidate C → arm64 `.so`, or return compiler diagnostics |
| `lab_diff` | differential-test original vs candidate over a corpus; verdict + first divergence |

All registered in `TOOLS` + `call_tool` + handlers in `omni-mcp/src/server.rs`;
existing tools (`call_function`, `intercept`, `read/write_mem`, `dump_module`, …)
are reused and gain an optional `session` argument.

## Data flow

```
apk ──lab_load_apk──▶ {modules, exports, unresolved}
                       │
          lab_corpus(arg_spec) ──▶ corpus_id
                       │
   lab_trace(target,args) ──▶ outcome + trace ──▶ [agent writes C]
                       │                                │
                       │                         lab_build(C) ──▶ candidate.so
                       │                                │
   lab_diff(original, candidate, corpus) ──▶ {match_ratio, first_divergence}
                       └──────────────── iterate ───────────────┘
```

## Testing strategy (TDD)

- **Fixtures:** begin with a self-contained function (libz `adler32`/`crc32` — a
  proven lab fixture) to validate C2/C4 without C1; then a libc-calling function
  (e.g. an `snprintf`/`memcpy` user) to exercise C1's provider resolution.
- **Unit tests:**
  - provider resolution: a target calling a sibling lib and a bionic function
    resolves and runs; a genuinely-absent import is reported in
    `unresolved`, not silently null.
  - `arg_spec` marshaling: in-buffers populated, out-buffers read back, scalars in
    the right registers.
  - corpus determinism: same seed → same vectors.
  - diff verdict: known-equal builds → full match; a one-byte mutation → mismatch
    with the correct first-divergence input and observable.
- **End-to-end:** take a small function whose C source we hold, compile it with
  C3, confirm `lab_diff` reports a full match against the original `.so`; mutate
  the source, confirm the divergence is caught.
- Follow the test-driven-development skill throughout.

## Decomposition / roadmap

- **Spec 1 (this one):** C1–C4, the isolated RE→rebuild→verify loop.
- **Spec 2 (later):** live ground-truth capture — a lightweight recorder of real
  function input→output from a running instance, to seed `lab_corpus` for
  context-dependent functions. Uses a minimal live-attach (read args/returns at a
  hooked address in the app's host process), **not** `frida-server`.
- **Spec 3 (later, optional):** coverage-guided fuzzing (use C2's branch trace to
  steer the corpus), batch RE across many exports, and orchestration so the loop
  runs with less agent hand-holding.

## Risks & open questions

- **Toolchain dependency (C3):** needs an arm64 clang/NDK on the host. Mitigation:
  runtime detection + a clear error; document the env. Not a hard blocker on
  machines that already build Android artifacts.
- **HLE import coverage:** `BoundaryBuilder` may not model every bionic function a
  target calls. Mitigation: residual unresolved imports are reported; the agent
  stubs them via `intercept`/`replace_return`, or we co-load the real `.so` from
  `sysroot/aosp-35`. Some targets will not be fully runnable until their imports
  are modeled — acceptable for Spec 1's scope.
- **Nondeterminism:** functions reading time, randomness, or TLS diverge
  spuriously under diffing. Mitigation: pin them through the provider (fixed
  clock/seed) or let the agent mark the observable `ignore` in `lab_diff`.
- **Memory/commit cost:** two full sessions + deps roughly double lab footprint;
  bounded, but note the Windows commit-limit sensitivity (do not run alongside a
  live game).
- **Open:** exact provider precedence when a sibling lib and the bionic HLE both
  define a symbol (proposal: co-loaded modules win, then HLE) — to confirm in
  planning.
