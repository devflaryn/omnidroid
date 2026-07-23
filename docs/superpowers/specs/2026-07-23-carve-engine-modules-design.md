# Carve `engine.py` into Modules — Design

**Date:** 2026-07-23
**Status:** Approved (brainstorming) — ready for implementation plan
**Thread:** B of the omnidroid improvement decomposition (A verified-liveness, B carve engine.py, C density/scale, D daemon+observability)

## Problem

`omnidroid/engine.py` is a 6,680-line god-module (311 KB) that does ~13
unrelated jobs: CLI I/O, base registry + dev-gate, port/process/instance
tracking, KSM memory dedup, QEMU command build + spawn, ADB primitives,
disk/partition surgery + base building, Roblox session/cookie/login, boot
orchestration, and every `cmd_*` handler. It is the one part of the package that
was never carved (`accounts.py`, `capture.py`, `vncview.py`, `farming.py`,
`measure.py`, `trimreg.py` already exist). Its size makes it hard to change
safely, hard to test in isolation, and the untested core behind the thin
existing test suite (22 files / ~2,700 LOC against 8,350 package LOC).

This thread is a **behavior-preserving refactor**: no functional change, no CLI
surface change. It exists to make Threads A, C, and D tractable.

## Constraints (discovered in the codebase)

1. **Wide implicit public API via attribute access.** 17 test files do
   `from omnidroid import engine as omni` and call `omni.<name>` directly.
   Dozens of functions and constants in `engine.py` are therefore an implicit
   public API. Moving names naively breaks every such test.
2. **Managed import cycle.** `engine` imports `config`; `config` lazily imports
   `engine` *inside a function* to avoid the cycle. The split must not tighten
   this.
3. **Single production entry point.** `cli.py` → `from omnidroid.engine import
   main`. Nothing else in production imports engine internals.
4. **Shared module state.** `_JSON_MODE` is a module global toggled by
   `enable_json_mode()` and read by `fail()`/`emit_json()`. Whichever module
   owns it must remain the single source, imported (not copied) elsewhere.

## Strategy: extract-behind-a-facade

The safe, textbook move for a god-module with an attribute-accessed API:

- Pull cohesive groups into new submodules.
- **`engine.py` remains a thin facade** that re-imports every moved name
  (`from omnidroid.runtime import *` plus explicit re-exports for
  underscore-prefixed names, which `*` does not carry). So `engine.<anything>`
  keeps resolving — **zero test churn, zero behavior change.**
- Extract **leaf-first** (lowest-coupling groups before their dependents),
  running the full suite green after *each* module so any regression bisects to
  one small move.

Rejected alternative: physically move names and rewrite all 17 test files'
`omni.<name>` references. More "honest" but multiplies risk and churn for no
functional gain. The facade can be tightened into real imports later if desired.

## Target module map (~8 new modules, medium granularity)

| # | New module | Responsibility | Key contents |
|---|---|---|---|
| 1 | `output.py` | CLI I/O + JSON mode (cross-cutting, zero deps) | `emit_json`, `enable_json_mode`, `fail`, `_JSON_MODE`, `redact_token` |
| 2 | `bases.py` | Base registry + dev-gate | `base_type`, `arch_of_base`, `autoregister_bases`, `effective_base_tag`, `_select_base_tag`, `_next_base_tag`, `visible_bases`, dev-gate fns, ARM/X86/DEV disk constants |
| 3 | `runtime.py` | Ports / process / instance tracking | `pid_alive`, `running_instances`, `running_pid`, `allocate_ports`, `_launch_lock`, `_reserve_ports`, `_wipe_runtime`, `_claimed_port_indices`, `runtime_dir`, host-mem helpers |
| 4 | `ksm.py` | KSM memory dedup | `ksm_*`, `cmd_ksm`, `cmd_bench_ksm`, `_ksm_wait_settle` |
| 5 | `qemu_proc.py` | QEMU command build + spawn + QMP | `qemu_command`, `qemu_command_arm`, `spawn_qemu`, `qmp`, `default_accel`, `machine_arg`, `check_accel`, `MODES`, `resolve_mode`, `_refresh_ephemeral_efivars` |
| 6 | `adb.py` | ADB / guest-shell primitives | `adb`, `adb_connect`, `adb_getprop`, `_require_adb_port`, `_pidof`, `_foreground` |
| 7 | `imaging.py` | Disk/partition surgery + base building | GPT/ext4/debugfs/`_lp_partition`, bootanim, `_silence_boot`, `_bake_apk_into_product`, `_patch_dev_boot`, `_stage_devkit_arm`, `build_dev_base`, `rebuild_base`, `update_kiosk*`, brand/bake cmds |
| 8 | `roblox.py` | Session / cookie / login / kiosk delivery | `validate_roblox_cookie`, `_await_bootstrap_login`, `resolve_token`, `store_session`, `roblox_deeplink`, `deliver_session`, `kiosk_broadcast`, `kiosk_installed`, `public_session`, `account_cookie`, `_validate_place_id`, ROBLOX_* constants |

**Stays in `engine.py`** (becomes the orchestration + CLI layer, ~2,000–2,500
lines, *composed of* the modules rather than *containing* everything):

- All `cmd_*` handlers and `main()` + argparse wiring.
- Boot/provision orchestration: `wait_for_boot`, `_ensure_booted`,
  `post_boot`, `provision_settings`, `lockdown_and_trim`,
  `_assert_kiosk_foreground`, `_devkit_activate`, `resolve_su`, `_magisk_pkg`.
- The re-export facade.

**Deliberate YAGNI:** `capture` / `vnc` / `install` cmd-wrappers stay in
`engine.py` for now — the heavy lifting already lives in `capture.py` /
`vncview.py`, so extracting the thin wrappers buys little.

## Extraction order (leaf → dependent)

`output` → `bases` → `ksm` → `adb` → `qemu_proc` → `runtime` → `imaging` →
`roblox`. Full suite green after each.

## Interaction with Thread A (verified-liveness)

Thread A rewrites exactly the code that becomes `runtime.py` (module 3), and its
"stamp identity at spawn" layer also edits `qemu_command`/`spawn_qemu` (module 5,
`qemu_proc.py`). A and B both touch these two future modules, so they are
**ordered, never run blind in parallel.**

**Chosen sequencing — interleave (B de-risks A, A still ships early):**

1. Extract the modules A does not touch: `output` → `bases` → `ksm` → `adb`
   (pure moves, suite green after each).
2. Extract `qemu_proc.py`, then `runtime.py` (still pure moves).
3. **Land Thread A** — implement verified-liveness inside the fresh
   `runtime.py` + `qemu_proc.py` (now ~200–400-line focused files, not a
   6,680-line haystack).
4. Extract the remaining independent modules: `imaging` → `roblox`.

Cost vs. "A-first": Thread A's spec references `engine.py` line locations that
move into `runtime.py` / `qemu_proc.py` — a trivial re-grounding, noted here and
to be noted in A's plan. Benefit: A's bug fix lands in clean code and still ships
before the whole refactor completes.

## Verification (the core safety of the refactor)

- **Full `pytest` suite green after every extraction step** — a red suite
  bisects to exactly one small move.
- **Import-equivalence assertion:** a test that `dir(engine)` (or an explicit
  captured name set) still exposes every name callers/tests rely on, so the
  facade cannot silently drop a re-export.
- **Behavior-preserving contract:** no `cmd_*` logic changes during B — pure
  relocation. Any behavior change belongs to Thread A or a later thread, never
  to an extraction step.

## Backward compatibility

- `engine.<name>` continues to resolve for all existing names (facade
  re-exports), so tests and `cli.py` are untouched.
- The `config` ↔ `engine` lazy-import cycle is preserved: `config`'s lazy
  `from omnidroid import engine` still works; new modules import `config`
  directly (as `engine` already does) and must not import back from `engine`
  at module top level (import from the new leaf modules or lazily) to avoid new
  cycles.
- `_JSON_MODE` lives in `output.py`; `engine` and all other modules import
  `output` and reference the single source — never copy the flag.

## Scope / YAGNI

- ~8 modules, medium granularity — not 13 micro-modules, not a single split.
- Facade re-exports, not a test-wide reference rewrite.
- `capture` / `vnc` / `install` wrappers stay in `engine.py`.
- No functional change of any kind — behavior belongs to other threads.

## Out of scope (other threads)

Thread A (verified-liveness) is a separate, already-approved spec. Threads C
(density/scale) and D (daemon + observability) are future specs. This thread is
pure structural extraction.
