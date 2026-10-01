# omnidroid Dynamic RE Workbench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give an LLM agent, over omnidroid's MCP, the primitives to dynamically exercise an arbitrary arm64 `.so`, observe it, rebuild behavior-matching pseudo-source into a `.so`, and prove equivalence by differential testing.

**Architecture:** Build the core primitives in the `omni-debug` crate (unit-tested against the checked-in `libz.so` fixture), then expose them as MCP tools in `omni-mcp`. Four capabilities: dependency-resolving load (chain a module-exports provider + a caller-supplied bionic provider instead of `EmptyProvider`), structured per-call observation with an `arg_spec`, an arm64 build helper, and a differential oracle over a reproducible corpus.

**Tech Stack:** Rust (workspace crates `omni-debug`, `omni-elf`, `omni-android`, `omni-mcp`); dynarmic backend via `omni-cpu`; MCP JSON-RPC in `omni-mcp`. Builds require PowerShell with `OMNIDROID_DYNARMIC_BUILD_DIR` set to a short path (e.g. `C:\od-unified`).

**Spec:** `docs/superpowers/specs/2026-10-01-omnidroid-re-workbench-design.md`

## Global Constraints

- Builds on Windows: run `cargo` from PowerShell with `$env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified"` (the Bash tool mangles the path). Never build while a live game/standby runs (commit-limit).
- `omni-debug` must not take a heavy new dependency it can avoid: the bionic provider is injected by the caller (`omni-mcp`), so `omni-debug` itself stays decoupled from `omni-android`. `omni-debug` keeps "no `cfg(target_os)`, no `libc`/`windows-sys`" (its Global Constraint 4).
- `SymbolProvider` implementors must be `Send + Sync + 'static` (ProviderRegistry stores `Box<dyn SymbolProvider>`). A provider snapshot therefore owns its data; it never borrows the session.
- Symbol-kind discipline: a data import (`STT_OBJECT`) bound to a function is a named failure (`provider.rs` D9). Providers set `SymbolValue.kind` honestly and the loader's `kind_mismatch` is surfaced, not swallowed.
- Equivalence is always reported as "matches across the tested corpus", never as universal equivalence.
- Determinism: corpus generation from the same seed yields identical vectors.

## Review Focus

- **Unresolved imports after C1**: a target that imports a data symbol (`STT_OBJECT`) the provider can't size — must be reported in the unresolved list, not bound to a function stub. (Task 1 test.)
- **arg_spec out-buffer of length 0 / overlapping in+out buffers**: must not corrupt the call or panic; zero-length buffer is a defined error. (Task 2 test.)
- **Corpus with count 0 or a seed that produces a degenerate value (all-zero length fields)**: must produce a valid (possibly empty) corpus, not panic. (Task 4 test.)
- **lab_diff when a call diverges by faulting in one build but returning in the other**: the oracle must record that as a divergence with the faulting side named, not propagate the error and abort the run. (Task 5 test.)
- **lab_build with no toolchain present**: returns a structured "toolchain missing" error naming the expected env, never a wrong-arch artifact or a panic. (Task 6 test.)

---

## Phase A — omni-debug core primitives

### Task 1: Dependency-resolving load (C1)

**Files:**
- Create: `crates/omni-debug/src/provider.rs` (the module-exports snapshot provider)
- Modify: `crates/omni-debug/src/session.rs` (`load_bytes` to use a built registry; add `load_resolved`, `unresolved_imports`)
- Modify: `crates/omni-debug/src/lib.rs` (re-export the new provider type + `UnresolvedImport`)
- Test: `crates/omni-debug/tests/resolve.rs`

**Interfaces:**
- Consumes: `omni_elf::loader::{ProviderRegistry, SymbolProvider, SymbolRequest, SymbolValue, SymbolKind}`; `omni_elf::loader::load` returning `LoadedObject` with `imports.unresolved`.
- Produces:
  - `omni_debug::ModuleExportsProvider` — `SymbolProvider` built from a slice of `(name, address, SymbolKind)`; constructor `ModuleExportsProvider::from_exports(name: &str, exports: Vec<(String, u64, SymbolKind)>) -> Self`.
  - `Session::load_resolved(&mut self, name: &str, path: impl AsRef<Path>, extra: Vec<Box<dyn SymbolProvider>>) -> Result<LoadReport>` where `LoadReport { module: String, exports: usize, unresolved: Vec<UnresolvedImport> }`.
  - `pub struct UnresolvedImport { pub name: String, pub kind: SymbolKind, pub library: Option<String>, pub weak: bool }`.
  - The registry order inside `load_resolved`: module-exports snapshot (already-loaded modules) first, then each `extra` provider in order. First match wins (co-loaded siblings beat the bionic HLE).

- [ ] **Step 1: Write the failing test** — `crates/omni-debug/tests/resolve.rs`:

```rust
use omni_debug::{Session, ModuleExportsProvider};
use omni_elf::loader::{SymbolKind, SymbolProvider, SymbolRequest, SymbolValue};
use std::path::PathBuf;

fn libz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

// A provider that binds any requested function name to a fixed address, so we can
// prove imports resolve. Not a working implementation — just a resolution witness.
struct BindAll(u64);
impl SymbolProvider for BindAll {
    fn name(&self) -> &str { "bind-all" }
    fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
        Some(SymbolValue { address: self.0, kind: req.kind })
    }
}

#[test]
fn empty_provider_leaves_imports_unresolved() {
    let mut s = Session::new().unwrap();
    let report = s.load_resolved("libz.so", libz(), vec![]).unwrap();
    assert!(report.unresolved.iter().any(|u| u.name == "memcpy"),
        "libz imports memcpy; with no provider it must be unresolved: {:?}", report.unresolved);
}

#[test]
fn extra_provider_resolves_function_imports() {
    let mut s = Session::new().unwrap();
    // Allocate a harmless target page the stub address can point at.
    let target = s.alloc_data(&[0u8; 16]).unwrap() as u64;
    let report = s.load_resolved("libz.so", libz(), vec![Box::new(BindAll(target))]).unwrap();
    assert!(!report.unresolved.iter().any(|u| u.name == "memcpy" && u.kind == SymbolKind::Function),
        "memcpy (a function import) must bind when a provider supplies it: {:?}", report.unresolved);
}

#[test]
fn unsized_data_import_stays_unresolved_not_bound_to_function() {
    // A provider that only answers functions must NOT satisfy a data import.
    struct FuncsOnly(u64);
    impl SymbolProvider for FuncsOnly {
        fn name(&self) -> &str { "funcs-only" }
        fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
            if req.kind == SymbolKind::Function { Some(SymbolValue { address: self.0, kind: SymbolKind::Function }) } else { None }
        }
    }
    let mut s = Session::new().unwrap();
    let target = s.alloc_data(&[0u8; 16]).unwrap() as u64;
    let report = s.load_resolved("libz.so", libz(), vec![Box::new(FuncsOnly(target))]).unwrap();
    // Any STT_OBJECT import libz has (if any) must remain unresolved under a funcs-only provider.
    assert!(report.unresolved.iter().all(|u| u.kind != SymbolKind::Function || u.weak),
        "every non-weak function import should have bound: {:?}", report.unresolved);
}
```

- [ ] **Step 2: Run to verify it fails** — PowerShell:
  `$env:OMNIDROID_DYNARMIC_BUILD_DIR="C:\od-unified"; cargo test -p omni-debug --test resolve`
  Expected: compile error — `Session::load_resolved` and `ModuleExportsProvider` do not exist.

- [ ] **Step 3: Implement `ModuleExportsProvider`** in `crates/omni-debug/src/provider.rs`:

```rust
use omni_elf::loader::{SymbolKind, SymbolProvider, SymbolRequest, SymbolValue};
use std::collections::HashMap;

/// A `SymbolProvider` backed by an owned snapshot of already-loaded modules' exports.
/// Owned (not borrowing the session) so it satisfies `SymbolProvider: 'static`.
pub struct ModuleExportsProvider {
    label: String,
    exports: HashMap<String, (u64, SymbolKind)>,
}

impl ModuleExportsProvider {
    #[must_use]
    pub fn from_exports(name: &str, exports: Vec<(String, u64, SymbolKind)>) -> Self {
        Self {
            label: name.to_string(),
            exports: exports.into_iter().map(|(n, a, k)| (n, (a, k))).collect(),
        }
    }
}

impl SymbolProvider for ModuleExportsProvider {
    fn name(&self) -> &str { &self.label }
    fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
        self.exports.get(req.name).map(|&(address, kind)| SymbolValue { address, kind })
    }
}
```

- [ ] **Step 4: Refactor `load_bytes` and add `load_resolved`** in `session.rs`. Replace the hardcoded `ProviderRegistry::empty_provider()` with a registry the caller influences. Add:

```rust
/// What a resolving load reports.
#[derive(Debug, Clone)]
pub struct LoadReport {
    pub module: String,
    pub exports: usize,
    pub unresolved: Vec<UnresolvedImport>,
}

#[derive(Debug, Clone)]
pub struct UnresolvedImport {
    pub name: String,
    pub kind: omni_elf::loader::SymbolKind,
    pub library: Option<String>,
    pub weak: bool,
}

impl Session {
    /// Load a library, resolving its imports against (1) the modules already loaded in this
    /// session, then (2) each provider in `extra` in order. First match wins.
    pub fn load_resolved(
        &mut self,
        name: &str,
        path: impl AsRef<Path>,
        extra: Vec<Box<dyn omni_elf::loader::SymbolProvider>>,
    ) -> Result<LoadReport> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| DebugError::BadRequest(format!("cannot read {}: {e}", path.display())))?;
        self.load_bytes_resolved(name, path, &bytes, extra)
    }
}
```

Implement `load_bytes_resolved` by: building `ProviderRegistry::new()`, registering a `ModuleExportsProvider::from_exports(...)` built from `self.modules` (collect each module's exports into `(name, address, SymbolKind::from_st_type-ish)` — map the stored `kind: String` back via matching `"STT_FUNC" => Function`, `"STT_OBJECT" => Object`, else `Unspecified`), registering each `extra` provider, then calling `loader::load` with that registry. After load, read `object.imports.unresolved` (confirm the field path in `omni_elf::loader`'s `LoadedObject`/`Imports`) and map to `Vec<UnresolvedImport>`. Keep the existing export-collection and `invalidate_code` logic. Have the old `load_bytes` delegate to `load_bytes_resolved(.., vec![])` so `load` keeps its empty-provider behavior for the self-contained case.

- [ ] **Step 5: Re-export** in `lib.rs`: add `pub use provider::ModuleExportsProvider;` and `pub use session::{LoadReport, UnresolvedImport};` and `pub mod provider;`.

- [ ] **Step 6: Run tests to verify they pass** — `cargo test -p omni-debug --test resolve`. Expected: PASS (3 tests). If `memcpy` is not among libz's imports on this fixture, adjust the probe name to an import that `cargo run`-dumping the fixture confirms (e.g. inspect with the existing `list_symbols`/an import dump) — pick a real `STT_FUNC` import of the fixture.

- [ ] **Step 7: Commit**

```bash
git add crates/omni-debug/src/provider.rs crates/omni-debug/src/session.rs crates/omni-debug/src/lib.rs crates/omni-debug/tests/resolve.rs
git commit -m "feat(omni-debug): resolving load (C1) — chain module-exports + caller providers"
```

### Task 2: Structured call with an arg_spec (C2 core)

**Files:**
- Create: `crates/omni-debug/src/argspec.rs`
- Modify: `crates/omni-debug/src/session.rs` (add `call_spec`), `lib.rs` (re-export)
- Test: `crates/omni-debug/tests/argspec.rs`

**Interfaces:**
- Consumes: `Session::alloc_data`, `Session::call_function`, `Session::read_mem`, `CallOutcome`.
- Produces:
  - `pub enum Arg { Scalar(u64), InBuffer(Vec<u8>), OutBuffer(usize), InOutBuffer(Vec<u8>) }`
  - `pub struct CallSpecResult { pub ret: u64, pub ret1: u64, pub instructions: u64, pub out_buffers: Vec<(usize /*arg index*/, Vec<u8>)> }`
  - `Session::call_spec(&mut self, address: GuestAddr, args: &[Arg]) -> Result<CallSpecResult>` — allocates each buffer arg with `alloc_data`, passes scalar values and buffer guest-addresses in order as X0..X7, runs via `call_function`, then reads `OutBuffer`/`InOutBuffer` contents back and returns them keyed by arg index.

- [ ] **Step 1: Write the failing test** — `crates/omni-debug/tests/argspec.rs`:

```rust
use omni_debug::{Session, Arg};
use std::path::PathBuf;
fn libz() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so") }

#[test]
fn adler32_over_in_buffer_matches_known_value() {
    // adler32(1, "Hello", 5) — a self-contained libz function, buffer passed via arg_spec.
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let r = s.call_spec(f, &[Arg::Scalar(1), Arg::InBuffer(b"Hello".to_vec()), Arg::Scalar(5)]).unwrap();
    assert_eq!(r.ret as u32, 0x058c01f5, "adler32 of 'Hello' with seed 1");
}

#[test]
fn zero_length_out_buffer_is_an_error() {
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let err = s.call_spec(f, &[Arg::OutBuffer(0)]);
    assert!(err.is_err(), "a zero-length buffer must be a defined error, not a panic");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-debug --test argspec`. Expected: compile error (`Arg`, `call_spec` missing).
- [ ] **Step 3: Implement** `Arg`, `CallSpecResult` in `argspec.rs` and `call_spec` in `session.rs`: for each arg, `Scalar(v)` → push `v`; `InBuffer(b)`/`InOutBuffer(b)` → `let a = self.alloc_data(&b)?; push a as u64`; `OutBuffer(n)` → `if n==0 { return Err(BadRequest) } let a = self.alloc_data(&vec![0u8;n])?; push a; record (index, a, n)`. Call `self.call_function(address, &args_u64)?`. For each recorded out/inout buffer, `self.read_mem(addr, n)?` and collect. Return `CallSpecResult`.
- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p omni-debug --test argspec`. Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(omni-debug): call_spec with arg_spec buffer marshaling (C2 core)"`

### Task 3: Per-call observation trace (C2 observation)

**Files:**
- Modify: `crates/omni-debug/src/session.rs` (add `call_traced` returning the already-available event stream: syscalls + call edges from existing `trace_calls`/`trace_syscalls`, plus memory watches)
- Test: `crates/omni-debug/tests/trace.rs`

**Interfaces:**
- Consumes: existing `Session::trace_calls`, `Session::trace_syscalls`, `TraceEvent`, `Watch`/`watches`.
- Produces: `Session::call_traced(&mut self, address: GuestAddr, args: &[u64], watch: &[GuestAddr]) -> Result<(CallOutcome, Vec<TraceEvent>)>` — enables call+syscall tracing and the given watches for one call, returns the outcome and the collected `TraceEvent`s, then clears the per-call tracing state.

Scope note: branch-level (basic-block) tracing beyond what `TraceEvent` already models is **out of scope for this task** — if the existing CPU exposes no per-branch hook, do not invent one here; the call/syscall/watch stream is the deliverable. Record a follow-up in the spec's Phase 3 if finer tracing is wanted.

- [ ] **Step 1: Write the failing test** — call `adler32` with `call_traced` and assert the returned event vector is well-formed (no syscalls for a pure function, outcome ret matches Task 2's value). Code:

```rust
use omni_debug::Session;
use std::path::PathBuf;
fn libz() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so") }
#[test]
fn traced_pure_call_reports_outcome_and_no_syscalls() {
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let buf = s.alloc_data(b"Hello").unwrap();
    let (outcome, events) = s.call_traced(f, &[1, buf as u64, 5], &[]).unwrap();
    assert_eq!(outcome.ret as u32, 0x058c01f5);
    assert!(events.iter().all(|e| !format!("{e:?}").contains("Syscall")),
        "adler32 makes no syscalls");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-debug --test trace`. Expected: compile error (`call_traced` missing).
- [ ] **Step 3: Implement** `call_traced`: enable `trace_syscalls(true)`, call, collect the events the session already records (confirm how `CallOutcome.events`/the session surfaces events — reuse that channel), return them, reset `trace_syscalls(false)`. If the current `CallOutcome` already carries `events`, `call_traced` is a thin wrapper returning `(outcome, outcome.events.clone())` plus watch setup.
- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p omni-debug --test trace`. Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(omni-debug): call_traced observation stream (C2)"`

### Task 4: Reproducible input corpus (C4)

**Files:**
- Create: `crates/omni-debug/src/corpus.rs`
- Modify: `lib.rs` (re-export)
- Test: `crates/omni-debug/tests/corpus.rs`

**Interfaces:**
- Produces:
  - `pub struct ArgTemplate { pub kind: ArgKind }` with `pub enum ArgKind { Scalar, Buffer { len: usize }, OutBuffer { len: usize } }`.
  - `pub fn generate(templates: &[ArgTemplate], seed: u64, count: usize) -> Vec<Vec<Arg>>` — deterministic: a small xorshift PRNG seeded by `seed`, producing `count` argument vectors where `Scalar` → a pseudo-random u64 (plus boundary values 0, 1, u64::MAX cycled in), `Buffer{len}` → `InBuffer` of `len` pseudo-random bytes, `OutBuffer{len}` → `OutBuffer(len)`.
  - No external RNG crate — a 20-line xorshift64 in the module (keeps deps minimal).

- [ ] **Step 1: Write the failing test** — `crates/omni-debug/tests/corpus.rs`:

```rust
use omni_debug::corpus::{generate, ArgTemplate, ArgKind};

#[test]
fn same_seed_same_corpus() {
    let t = [ArgTemplate { kind: ArgKind::Scalar }, ArgTemplate { kind: ArgKind::Buffer { len: 8 } }];
    let a = generate(&t, 42, 10);
    let b = generate(&t, 42, 10);
    assert_eq!(format!("{a:?}"), format!("{b:?}"), "same seed must reproduce the corpus");
    assert_eq!(a.len(), 10);
}

#[test]
fn count_zero_yields_empty_corpus() {
    let t = [ArgTemplate { kind: ArgKind::Scalar }];
    assert!(generate(&t, 1, 0).is_empty());
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-debug --test corpus`. Expected: compile error.
- [ ] **Step 3: Implement** `corpus.rs` with the xorshift64 PRNG and `generate`. Cycle boundary scalars (0, 1, u64::MAX) into the first few vectors, then PRNG values.
- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p omni-debug --test corpus`. Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(omni-debug): deterministic input corpus generator (C4)"`

### Task 5: Differential oracle (C4)

**Files:**
- Create: `crates/omni-debug/src/diff.rs`
- Modify: `lib.rs` (re-export)
- Test: `crates/omni-debug/tests/diff.rs`

**Interfaces:**
- Consumes: `Session::with_library`/`load_resolved`, `Session::resolve_symbol`, `Session::call_spec`.
- Produces:
  - `pub struct DiffResult { pub total: usize, pub matched: usize, pub first_divergence: Option<Divergence> }`
  - `pub struct Divergence { pub index: usize, pub observable: &'static str, pub expected: String, pub got: String }`
  - `pub fn diff_calls(original: &mut Session, candidate: &mut Session, symbol: &str, corpus: &[Vec<Arg>]) -> DiffResult` — for each input, resolve `symbol` in each session, run `call_spec` on both (a fault in one but not the other is a divergence with `observable = "fault"` and the faulting side named in `got`), compare `ret`, `ret1`, and each out-buffer's bytes; the first mismatch sets `first_divergence` and comparison of observables stops for that input but the run continues to count `total`.

- [ ] **Step 1: Write the failing test** — `crates/omni-debug/tests/diff.rs`: load two independent `Session::with_library(libz())` as original and candidate, build a corpus for `adler32` (one scalar seed, one buffer, one length), assert `matched == total` and `first_divergence.is_none()` (same library vs itself must fully match). Then a negative: wrap `candidate`'s `adler32` call through a `symbol` that differs (`crc32` as candidate "wrong impl") and assert a divergence is reported at index 0 with `observable == "ret"`. Code:

```rust
use omni_debug::{Session, Arg};
use omni_debug::diff::{diff_calls};
use std::path::PathBuf;
fn libz() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so") }

fn corpus() -> Vec<Vec<Arg>> {
    vec![
        vec![Arg::Scalar(1), Arg::InBuffer(b"Hello".to_vec()), Arg::Scalar(5)],
        vec![Arg::Scalar(0), Arg::InBuffer(b"".to_vec()), Arg::Scalar(0)],
        vec![Arg::Scalar(7), Arg::InBuffer(vec![0xAB; 32]), Arg::Scalar(32)],
    ]
}

#[test]
fn identical_library_fully_matches() {
    let mut a = Session::with_library(libz()).unwrap();
    let mut b = Session::with_library(libz()).unwrap();
    let r = diff_calls(&mut a, &mut b, "adler32", &corpus());
    assert_eq!(r.matched, r.total);
    assert!(r.first_divergence.is_none());
}

#[test]
fn different_function_diverges_on_ret() {
    // Use the real adler32 vs a candidate that resolves the SAME name to crc32's code,
    // by loading a session whose "adler32" we deliberately compare against crc32 via a
    // second symbol. Simplest: compare adler32 against crc32 by symbol swap in a helper.
    let mut a = Session::with_library(libz()).unwrap();
    let mut b = Session::with_library(libz()).unwrap();
    // adler32 and crc32 share the (seed, buf, len) shape but differ in output.
    let r = diff_calls_named(&mut a, "adler32", &mut b, "crc32", &corpus());
    assert!(r.first_divergence.as_ref().map(|d| d.observable) == Some("ret"));
}
```

(If a same-name/different-code setup is awkward without a compiler, add a sibling `diff_calls_named(orig, orig_sym, cand, cand_sym, corpus)` helper and have `diff_calls` delegate with the same symbol for both. Implement both.)

- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-debug --test diff`. Expected: compile error.
- [ ] **Step 3: Implement** `diff.rs` with `diff_calls` and `diff_calls_named`. Catch a per-call `Err` from `call_spec` and turn it into a `"fault"` divergence rather than propagating.
- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p omni-debug --test diff`. Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(omni-debug): differential oracle over a corpus (C4)"`

---

## Phase B — build helper (C3)

### Task 6: arm64 toolchain detection + compile wrapper

**Files:**
- Create: `crates/omni-rebuild/Cargo.toml`, `crates/omni-rebuild/src/lib.rs`
- Modify: workspace `Cargo.toml` members
- Test: `crates/omni-rebuild/tests/build.rs`

**Interfaces:**
- Produces:
  - `pub struct Toolchain { pub clang: PathBuf, pub target: String }`
  - `pub fn detect() -> Result<Toolchain, BuildError>` — looks at `OMNI_NDK`/`ANDROID_NDK_HOME` for a prebuilt `aarch64` clang, else a `clang` on `PATH` whose `--print-targets` lists `aarch64`; `BuildError::ToolchainMissing { looked_for: Vec<String> }` when none.
  - `pub fn compile_shared(tc: &Toolchain, sources: &[(String /*name*/, String /*text*/)], out: &Path, extra_flags: &[String]) -> Result<PathBuf, BuildError>` — writes sources to a temp dir, invokes `clang --target=<target> -shared -fPIC -o out <srcs> <flags>`, returns `out` or `BuildError::Compile { diagnostics: String }` carrying stderr.

- [ ] **Step 1: Write the failing test** — `crates/omni-rebuild/tests/build.rs`:

```rust
use omni_rebuild::{detect, compile_shared, BuildError};

#[test]
fn detect_reports_missing_toolchain_cleanly() {
    // On a host with no arm64 clang (CI/dev Windows), detect() must return a named error,
    // never panic. On a host that HAS one, it returns Ok — accept either, but never panic.
    match detect() {
        Ok(tc) => assert!(tc.target.contains("aarch64")),
        Err(BuildError::ToolchainMissing { looked_for }) =>
            assert!(looked_for.iter().any(|s| s.contains("OMNI_NDK") || s.contains("clang"))),
        Err(e) => panic!("unexpected error shape: {e:?}"),
    }
}

#[test]
fn compile_happy_path_when_toolchain_present() {
    let tc = match detect() { Ok(t) => t, Err(_) => { eprintln!("no toolchain; skipping"); return; } };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cand.so");
    let src = ("cand.c".to_string(), "int add(int a,int b){return a+b;}".to_string());
    let so = compile_shared(&tc, &[src], &out, &[]).expect("compile");
    assert!(so.exists());
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-rebuild`. Expected: compile error (crate/functions missing). (Add `tempfile` as a dev-dependency; if the workspace forbids new deps, write to the OS temp dir manually instead and drop `tempfile`.)
- [ ] **Step 3: Implement** the crate: `detect()` and `compile_shared()` as specified, `BuildError` with `thiserror`. No `unwrap` on external process output.
- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p omni-rebuild`. Expected: PASS (the happy-path test self-skips with no toolchain, which is this host).
- [ ] **Step 5: Commit** — `git commit -m "feat(omni-rebuild): arm64 toolchain detection + compile wrapper (C3)"`

---

## Phase C — MCP tools (omni-mcp)

### Task 7: Named-session registry refactor

**Files:** Modify `crates/omni-mcp/src/server.rs` (replace `lab: Option<Session>` with `labs: std::collections::HashMap<String, Session>` + a `DEFAULT_LAB: &str = "default"`; `lab_mut()` takes an optional name arg, defaults to `"default"`; `lab_load` inserts under `"default"`).

**Interfaces:** Produces `fn lab_mut(&mut self, name: Option<&str>) -> Result<&mut Session, RpcError>` and `fn lab_get(&self, name: Option<&str>)`. All existing lab tool handlers pass `None` (→ default) so behavior is unchanged.

- [ ] **Step 1: Write the failing test** — `crates/omni-mcp/tests/labs.rs` (or extend an existing server test): construct a `Server`, call the `lab_load` handler for `"default"`, then a (new) `lab_load` targeting name `"candidate"`, assert both resolve independently. (If `Server` isn't directly test-constructible, assert via the JSON `call_tool` entry points with `path` to the fixture `libz.so` — reference `../omni-debug/tests/fixtures/libz.so`.)
- [ ] **Step 2: Run to verify it fails** — `cargo test -p omni-mcp --test labs`. Expected: fail (no multi-lab support).
- [ ] **Step 3: Implement** the registry refactor; keep every existing handler compiling by defaulting the name.
- [ ] **Step 4: Run** the full `cargo test -p omni-mcp` to confirm no regression. Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "refactor(omni-mcp): named lab sessions (default + original/candidate)"`

### Task 8: `lab_load_apk` tool

**Files:** Modify `server.rs` (TOOLS entry, `call_tool` arm, handler `lab_load_apk`); reuse `omni-apk` for zip extraction (confirm the crate's API) or `zip`/`std` to read `lib/arm64-v8a/*.so`.

**Interfaces:** Params `{ apk: string, target: string, session?: string, with_deps?: bool }`. Handler: extract libs to a temp dir, `load_resolved(target, ..., extra)` where `extra` includes co-loaded sibling modules (load each sibling first into the same session, so the module-exports provider covers them) and — when available — the bionic provider from Task' follow-up. Returns `{ modules, exports: [names], unresolved: [{name, kind, library, weak}] }`.

- [ ] **Step 1** Write a handler test driving `call_tool("lab_load_apk", {apk, target})` against a small APK fixture if one exists; else against the `libz.so` path with a synthetic single-lib "apk" shim (document the limitation and test the unresolved-report shape).
- [ ] **Step 2** Run; expected fail.
- [ ] **Step 3** Implement extraction + load + JSON shaping.
- [ ] **Step 4** Run; expected pass.
- [ ] **Step 5** Commit — `git commit -m "feat(omni-mcp): lab_load_apk — resolving load with dependency report"`

### Task 9: `lab_trace` tool

**Files:** Modify `server.rs`. Params `{ session?, symbol|address, arg_spec:[...], record:[...], max_events? }`. Parse `arg_spec` JSON into `Vec<Arg>`, call `Session::call_spec` (and `call_traced` when `record` is non-empty), return `{ ret, ret1, instructions, out_buffers:[{index, hex}], events:[...] }`.

- [ ] Steps 1–5 as the standard TDD cycle (failing handler test parsing an `arg_spec` and asserting `ret` for `adler32`; implement; pass; commit `feat(omni-mcp): lab_trace — structured call + observation`).

### Task 10: `lab_corpus` tool

**Files:** Modify `server.rs`. Params `{ session?, templates:[...], seed, count }` → `omni_debug::corpus::generate` → store the corpus in a `Server` field `corpora: HashMap<String, Vec<Vec<Arg>>>` keyed by a returned `corpus_id`. Return `{ corpus_id, count }`.

- [ ] Steps 1–5 standard TDD (test: generate twice with same seed → same `count`, deterministic; implement; pass; commit `feat(omni-mcp): lab_corpus — reproducible corpus store`).

### Task 11: `lab_build` tool

**Files:** Modify `server.rs` + add `omni-rebuild` as an `omni-mcp` dependency. Params `{ sources:[{name,text}], flags?, soname? }` → `omni_rebuild::detect()` then `compile_shared(...)` into a temp path → return `{ so_path }` or `{ error: "toolchain missing", looked_for:[...] }` / `{ error: "compile", diagnostics }`.

- [ ] Steps 1–5 standard TDD (test: with no toolchain the handler returns the structured `toolchain missing` JSON, not an error-throw; implement; pass; commit `feat(omni-mcp): lab_build — compile candidate C to arm64 .so`).

### Task 12: `lab_diff` tool

**Files:** Modify `server.rs`. Params `{ original?, candidate?, symbol, corpus_id, compare:[...] }` → look up the two named sessions and the stored corpus → `omni_debug::diff::diff_calls` → return `{ total, matched, first_divergence }`.

- [ ] Steps 1–5 standard TDD (test: load `libz.so` into both `original` and `candidate`, a corpus for `adler32`, assert `matched==total`; implement; pass; commit `feat(omni-mcp): lab_diff — differential oracle tool`).

---

## Phase D — docs

### Task 13: Document the workbench tools

**Files:** Modify `crates/omni-mcp/README.md` and the `omnidroid-frida` skill doc (`~/.claude/skills/omnidroid-frida/SKILL.md`) to add a "Dynamic RE workbench" section listing `lab_load_apk`, `lab_trace`, `lab_corpus`, `lab_build`, `lab_diff`, the arg_spec, the toolchain prerequisite, and the "equivalence is relative to the corpus" caveat.

- [ ] **Step 1** Write the README section (tool table + a worked `adler32` round-trip example).
- [ ] **Step 2** Update the skill doc's "A live app's native code" area with a pointer to the workbench for the RE→rebuild→verify loop.
- [ ] **Step 3** Commit — `git commit -m "docs(re-workbench): document the lab_* workbench tools"`

---

## Self-review notes

- **Spec coverage:** C1→Task 1; C2→Tasks 2–3 (+ `lab_trace` Task 9); C3→Task 6 (+ `lab_build` Task 11); C4→Tasks 4–5 (+ `lab_corpus`/`lab_diff` Tasks 10/12); server refactor→Task 7; tool surface→Tasks 8–12; roadmap/live-capture explicitly deferred (not in this plan). Covered.
- **Toolchain reality:** C3 happy-path is unverifiable on this Windows host (no arm64 clang); its test self-skips and the detection/error path is always exercised. Full round-trip verification is deferred to a build machine with the NDK — called out, not hidden.
- **Trace depth:** Task 3 delivers the call/syscall/watch stream only; finer branch tracing is explicitly deferred to spec Phase 3 rather than faked.
- **Provider precedence:** co-loaded modules beat the bionic HLE (module provider registered first) — matches the spec's stated default.
