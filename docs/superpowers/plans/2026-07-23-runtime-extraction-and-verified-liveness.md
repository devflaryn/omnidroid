# Runtime Extraction + Verified Liveness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the leaf modules `engine.py` Thread A depends on, then implement verified-liveness so a live QEMU is never mis-detected and its port never re-issued.

**Architecture:** Behavior-preserving extract-behind-a-facade refactor first (`output`, `bases`, `ksm`, `adb`, `qemu_proc`, `runtime` become their own modules; `engine.py` re-exports every moved name so `engine.<x>` keeps resolving). Then verified-liveness lands inside the fresh `runtime.py`/`qemu_proc.py`: record the already-emitted `-name omni-<name>` token in `run.json`, replace bare `pid_alive` with an identity-verifying `instance_live()`, add a probe-before-allocate safety net, and a `reconcile_runtime()` self-heal sweep.

**Tech Stack:** Python 3, pytest, QEMU (QMP over TCP), POSIX `/proc`, existing `qmp()`/`pid_alive()` helpers in `omnidroid/engine.py`.

## Global Constraints

- **Behavior-preserving extraction:** Tasks 2–7 relocate code only. No `cmd_*` logic change, no CLI-surface change. Any behavior change belongs to Tasks 8–12.
- **Facade rule:** `engine.py` must keep exposing every moved name via re-export (`from omnidroid.<mod> import *` carries public names; underscore-prefixed names need an explicit re-import line). Verified by the import-equivalence test (Task 1).
- **Suite green after every task:** `python -m pytest -q` must pass before each commit. A red suite bisects to exactly one task.
- **Import-cycle rule:** New modules import `omnidroid.config` directly (as `engine` already does). They must NOT `from omnidroid import engine` at module top level — import from the new leaf modules, or lazily inside a function, to avoid re-creating the managed `config`↔`engine` cycle.
- **`_JSON_MODE` single source:** it lives in `output.py`; `engine` and all modules import `output` and reference `output._JSON_MODE` / `output.fail` — never copy the flag.
- **Identity token:** the instance identity is the string `f"omni-{name}"`, already emitted by both `qemu_command` and `qemu_command_arm` as `-name omni-<name>`. Do not invent a new token; reuse this one.

---

## Shared extraction procedure (Tasks 2–7)

Each extraction task moves a named set of defs/constants out of `engine.py` into a new module and re-exports them from the facade. The mechanical steps are identical; only the module name and the name-set differ. For every extraction task:

1. Create `omnidroid/<module>.py` with the import header shown in the task.
2. **Cut** each named def/constant from `engine.py` and paste it verbatim into the new module (do not edit bodies).
3. In `engine.py`, at the location the block used to occupy, add the facade line(s): `from omnidroid.<module> import *` plus one explicit `from omnidroid.<module> import _name1, _name2, ...` for every underscore-prefixed moved name.
4. If a moved function references a name that now lives in another new module or stays in `engine`, add the needed `from omnidroid.<other> import ...` to the new module's header. Circular top-level imports are forbidden (see Global Constraints) — if one would arise, import lazily inside the function.
5. Run `python -m pytest -q` — expect all green (unchanged count).
6. Run the import-equivalence test `python -m pytest tests/test_facade_equivalence.py -q` — expect green.
7. Commit.

---

### Task 1: Import-equivalence safety net

**Files:**
- Create: `tests/test_facade_equivalence.py`

**Interfaces:**
- Produces: a frozen snapshot of `engine`'s public name set that later tasks assert against, so the facade can never silently drop a re-export.

- [ ] **Step 1: Write the test that captures and enforces the name set**

```python
# tests/test_facade_equivalence.py
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from omnidroid import engine  # noqa: E402

SNAPSHOT = pathlib.Path(__file__).with_name("engine_public_names.json")

# Names the test suite and cli.py reach for via attribute access. Anything
# in this set MUST stay resolvable as engine.<name> after every extraction.
def _current_names():
    return sorted(n for n in dir(engine) if not n.startswith("__"))

def test_facade_exposes_every_snapshot_name():
    if not SNAPSHOT.exists():
        SNAPSHOT.write_text(json.dumps(_current_names(), indent=2))
    expected = set(json.loads(SNAPSHOT.read_text()))
    missing = expected - set(_current_names())
    assert not missing, f"facade dropped names: {sorted(missing)}"
```

- [ ] **Step 2: Generate the snapshot against the current (pre-extraction) engine**

Run: `python -m pytest tests/test_facade_equivalence.py -q`
Expected: PASS (first run writes `tests/engine_public_names.json` capturing today's names, then asserts none missing).

- [ ] **Step 3: Confirm the snapshot file was written and is non-trivial**

Run: `python -c "import json; print(len(json.load(open('tests/engine_public_names.json'))))"`
Expected: a number in the hundreds (every public engine name). If it prints 0, delete the file and re-run Step 2.

- [ ] **Step 4: Commit**

```bash
git add tests/test_facade_equivalence.py tests/engine_public_names.json
git commit -m "test: freeze engine public-name set as facade safety net"
```

---

### Task 2: Extract `output.py` (CLI I/O + JSON mode)

**Files:**
- Create: `omnidroid/output.py`
- Modify: `omnidroid/engine.py` (remove moved block, add facade lines)

**Interfaces:**
- Produces: `output.emit_json`, `output.enable_json_mode`, `output.fail`, `output._JSON_MODE`, `output.redact_token`. `engine.fail` etc. remain via facade.

**Names to move:** `emit_json`, `_JSON_MODE`, `enable_json_mode`, `fail`, `redact_token`.

- [ ] **Step 1: Create the module with its header**

```python
# omnidroid/output.py
"""CLI output primitives: the single JSON payload channel, --json mode, and
the contract-shaped fatal-error helper. Cross-cutting, zero omnidroid deps."""
import sys
import json
```

- [ ] **Step 2: Move the five names** (follow Shared extraction procedure steps 2–4). Paste `emit_json`, `_JSON_MODE`, `enable_json_mode`, `fail`, `redact_token` verbatim into `output.py`. In `engine.py` replace them with:

```python
from omnidroid.output import emit_json, enable_json_mode, fail, redact_token
from omnidroid import output   # for output._JSON_MODE single-source reads
from omnidroid.output import _JSON_MODE  # noqa: F401  (facade re-export)
```

Note: any `engine` code that *sets* json mode already calls `enable_json_mode()`, which mutates `output._JSON_MODE`. Reads inside `fail` now live in `output`, so no engine-side read of the flag remains — the `_JSON_MODE` re-export exists only to satisfy the facade snapshot.

- [ ] **Step 3: Run the full suite**

Run: `python -m pytest -q`
Expected: PASS, same test count as Task 1.

- [ ] **Step 4: Run the facade test**

Run: `python -m pytest tests/test_facade_equivalence.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/output.py omnidroid/engine.py
git commit -m "refactor: extract output.py (json mode + fail) behind engine facade"
```

---

### Task 3: Extract `bases.py` (base registry + dev-gate)

**Files:**
- Create: `omnidroid/bases.py`
- Modify: `omnidroid/engine.py`

**Interfaces:**
- Consumes: `output.fail`, `omnidroid.config`.
- Produces: base-registry + dev-gate names via facade.

**Names to move:** `base_type`, `acct_base_is_arm`, `base_is_dev`, `dev_mode_enabled`, `_truthy_env`, `_dev_mode_for_play`, `visible_bases`, `assert_dev_allowed`, `acct_is_dev`, `arch_of_base`, `acct_arch`, `arm_edk2_code`, `effective_base_tag`, `base_missing_files`, `autoregister_bases`, `base_setup_help`, `_select_base_tag`, `_next_base_tag`, and the base constants (`BASE_TYPE_X86`, `BASE_TYPE_ARM`, `DEV_MODE_ENV`, `ARM_BASE_*`, `X86_BASE_*`, `DEV_BASE_TAG`, `ARM_DEVKIT_DISK`, `ARM_DEVSYSTEM_DISK`, `ROOTED_MARKER`, `ROOT_PENDING_MARKER`, `ARM_DEVDATA_DISK`, `DEVKIT_MOUNT`, `DEVKIT_WORK`, `DEVKIT_MANIFEST_GUEST`, `ARM_EDK2_CANDIDATES`). Leave `cmd_bases`/`cmd_use_base` in `engine.py` (they are command handlers).

- [ ] **Step 1: Create the module header**

```python
# omnidroid/bases.py
"""Base-image registry, arch resolution, and the dev-base gate."""
import os
from pathlib import Path

from omnidroid import config
from omnidroid.output import fail
```

- [ ] **Step 2: Move the names** per the Shared extraction procedure (steps 2–4). Facade lines in `engine.py`:

```python
from omnidroid.bases import *  # noqa: F401,F403
from omnidroid.bases import (_truthy_env, _dev_mode_for_play, _select_base_tag,
                             _next_base_tag)  # noqa: F401
```

- [ ] **Step 3: Run the suite** — `python -m pytest -q` — Expected: PASS.
- [ ] **Step 4: Facade test** — `python -m pytest tests/test_facade_equivalence.py -q` — Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add omnidroid/bases.py omnidroid/engine.py
git commit -m "refactor: extract bases.py (registry + dev-gate) behind engine facade"
```

---

### Task 4: Extract `ksm.py` (KSM memory dedup)

**Files:**
- Create: `omnidroid/ksm.py`
- Modify: `omnidroid/engine.py`

**Names to move:** `KSM_DIR`, `PAGE_SIZE`, `ksm_available`, `ksm_stats`, `ksm_write`, `ksm_saved_mb`, `pid_ksm_merged_mb`, `_ksm_wait_settle`. Leave `cmd_ksm`, `cmd_bench_ksm`, `_wait_game_running` in `engine.py` (command handlers / boot helper).

- [ ] **Step 1: Create the module header**

```python
# omnidroid/ksm.py
"""Kernel Same-page Merging stats + controls (Linux /sys/kernel/mm/ksm)."""
from pathlib import Path

KSM_DIR = Path("/sys/kernel/mm/ksm")
PAGE_SIZE = 4096
```

(Move the two constants into the header above rather than re-declaring; delete their originals from `engine.py`.)

- [ ] **Step 2: Move the functions** per the Shared extraction procedure. Facade lines in `engine.py`:

```python
from omnidroid.ksm import *  # noqa: F401,F403
from omnidroid.ksm import _ksm_wait_settle  # noqa: F401
```

- [ ] **Step 3: Run the suite** — `python -m pytest -q` — Expected: PASS.
- [ ] **Step 4: Facade test** — `python -m pytest tests/test_facade_equivalence.py -q` — Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add omnidroid/ksm.py omnidroid/engine.py
git commit -m "refactor: extract ksm.py behind engine facade"
```

---

### Task 5: Extract `adb.py` (ADB / guest-shell primitives)

**Files:**
- Create: `omnidroid/adb.py`
- Modify: `omnidroid/engine.py`

**Names to move:** `_require_adb_port`, `adb`, `adb_connect`, `adb_getprop`, `_pidof`, `_foreground`.

- [ ] **Step 1: Create the module header**

```python
# omnidroid/adb.py
"""ADB transport + guest-shell primitives keyed off an account's adb_port."""
import subprocess

from omnidroid.output import fail
```

- [ ] **Step 2: Move the functions** per the Shared extraction procedure. `_require_adb_port` calls `fail` (now imported). Facade lines in `engine.py`:

```python
from omnidroid.adb import *  # noqa: F401,F403
from omnidroid.adb import _require_adb_port, _pidof, _foreground  # noqa: F401
```

- [ ] **Step 3: Run the suite** — `python -m pytest -q` — Expected: PASS.
- [ ] **Step 4: Facade test** — `python -m pytest tests/test_facade_equivalence.py -q` — Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add omnidroid/adb.py omnidroid/engine.py
git commit -m "refactor: extract adb.py behind engine facade"
```

---

### Task 6: Extract `qemu_proc.py` (QEMU command build + spawn + QMP)

**Files:**
- Create: `omnidroid/qemu_proc.py`
- Modify: `omnidroid/engine.py`

**Interfaces:**
- Consumes: `omnidroid.config`, `bases.base_type`/`BASE_TYPE_ARM`, `runtime.runtime_dir` (Task 7 lands after this — see cycle note).
- Produces: `qemu_command`, `qemu_command_arm`, `spawn_qemu`, `qmp`, `default_accel`, `machine_arg`, `check_accel`, `MODES`, `DEFAULT_MODE`, `resolve_mode`, `_refresh_ephemeral_efivars` via facade.

**Names to move:** `default_accel`, `_gl_window_requested`, `machine_arg`, `check_accel`, `MODES`, `DEFAULT_MODE`, `resolve_mode`, `_assert_port_triple`, `qemu_command_arm`, `qemu_command`, `_refresh_ephemeral_efivars`, `spawn_qemu`, `qmp`.

> **Cycle note:** `spawn_qemu` and `_refresh_ephemeral_efivars` call `runtime_dir` / `account_dir`, which are still in `engine.py` at this point (Task 7 moves `runtime_dir`). To avoid a top-level cycle, `qemu_proc.py` imports those lazily inside the functions that use them: `from omnidroid.engine import runtime_dir, account_dir` placed at the top of the function body, not the module. After Task 7, change that lazy import to `from omnidroid.runtime import runtime_dir` (still lazy is fine).

- [ ] **Step 1: Create the module header**

```python
# omnidroid/qemu_proc.py
"""QEMU command construction, process spawn, and QMP monitor access."""
import json
import socket
import subprocess
import time
from pathlib import Path

from omnidroid import config
from omnidroid.bases import base_type, BASE_TYPE_ARM
```

- [ ] **Step 2: Move the functions/constants** per the Shared extraction procedure, applying the lazy-import cycle note for `spawn_qemu`/`_refresh_ephemeral_efivars`. Facade lines in `engine.py`:

```python
from omnidroid.qemu_proc import *  # noqa: F401,F403
from omnidroid.qemu_proc import (_gl_window_requested, _assert_port_triple,
                                 _refresh_ephemeral_efivars)  # noqa: F401
```

- [ ] **Step 3: Run the suite** — `python -m pytest -q` — Expected: PASS (see `tests/test_gl_spike.py`, `tests/test_version_modes.py`, `tests/test_ephemeral_boot.py` exercise these paths).
- [ ] **Step 4: Facade test** — `python -m pytest tests/test_facade_equivalence.py -q` — Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add omnidroid/qemu_proc.py omnidroid/engine.py
git commit -m "refactor: extract qemu_proc.py (command build + spawn + qmp) behind engine facade"
```

---

### Task 7: Extract `runtime.py` (ports / process / instance tracking)

**Files:**
- Create: `omnidroid/runtime.py`
- Modify: `omnidroid/engine.py`; `omnidroid/qemu_proc.py` (retarget the lazy `runtime_dir` import)

**Interfaces:**
- Consumes: `omnidroid.config`, `qemu_proc.qmp` (Task 8+ use it, imported lazily to avoid a cycle since `qemu_proc.spawn_qemu` imports `runtime_dir`).
- Produces: `pid_alive`, `running_instances`, `running_pid`, `allocate_ports`, `_launch_lock`, `_reserve_ports`, `_wipe_runtime`, `_claimed_port_indices`, `runtime_dir`, `host_rss_mb`, `host_mem_available_mb`, `IS_WINDOWS` (if defined here) via facade.

**Names to move:** `runtime_dir`, `_reserve_ports`, `_wipe_runtime`, `running_instances`, `_claimed_port_indices`, `host_rss_mb`, `host_mem_available_mb`, `pid_alive`, `running_pid`, `allocate_ports`, `_launch_lock`, `vnc_start`, `_require_adb_port` stays in `adb`. Keep `_assert_port_triple` in `qemu_proc` (already moved).

- [ ] **Step 1: Create the module header**

```python
# omnidroid/runtime.py
"""Per-instance runtime tracking: port allocation, run.json reservations,
and process-liveness checks. run.json is the claim; the running QEMU is the
truth (see verified-liveness, Tasks 8-12)."""
import contextlib
import json
import os
import time

from omnidroid import config
```

(Move `IS_WINDOWS` detection here only if it is defined in `engine.py`; otherwise import it. Check with `grep -n "IS_WINDOWS *=" omnidroid/engine.py` and place the single definition in `runtime.py`, re-exported via facade, since `pid_alive` needs it.)

- [ ] **Step 2: Move the functions** per the Shared extraction procedure. Then in `qemu_proc.py` change the in-function lazy import to `from omnidroid.runtime import runtime_dir` (and `from omnidroid.engine import account_dir` stays lazy — `account_dir` is not being moved in this plan). Facade lines in `engine.py`:

```python
from omnidroid.runtime import *  # noqa: F401,F403
from omnidroid.runtime import (_reserve_ports, _wipe_runtime,
                               _claimed_port_indices, _launch_lock)  # noqa: F401
```

- [ ] **Step 3: Run the suite** — `python -m pytest -q` — Expected: PASS (`tests/test_runtime.py` directly exercises this module).
- [ ] **Step 4: Facade test** — `python -m pytest tests/test_facade_equivalence.py -q` — Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add omnidroid/runtime.py omnidroid/qemu_proc.py omnidroid/engine.py
git commit -m "refactor: extract runtime.py (ports + liveness) behind engine facade"
```

---

### Task 8: Record the identity token in `run.json`

**Files:**
- Modify: `omnidroid/qemu_proc.py` (`spawn_qemu`)
- Modify: `omnidroid/runtime.py` (add `expected_identity`)
- Test: `tests/test_verified_liveness.py`

**Interfaces:**
- Produces: `runtime.expected_identity(rec) -> str` returning `f"omni-{rec['name']}"`; `run.json` now carries `"identity": "omni-<name>"`.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_verified_liveness.py
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from omnidroid import runtime  # noqa: E402

def test_expected_identity_matches_qemu_name_flag():
    # The -name flag both qemu_command paths emit is f"omni-{name}".
    assert runtime.expected_identity({"name": "acc0"}) == "omni-acc0"

def test_run_json_records_identity(tmp_path, monkeypatch):
    # spawn_qemu writes run.json with an identity field equal to the -name token.
    from omnidroid import qemu_proc
    monkeypatch.setattr(qemu_proc, "check_accel", lambda: None)
    monkeypatch.setattr(qemu_proc, "qemu_command", lambda *a, **k: ["true"])
    monkeypatch.setattr(runtime, "runtime_dir", lambda name: tmp_path / name)
    # qemu_proc.runtime_dir is imported lazily from runtime; patch there too.
    monkeypatch.setattr("omnidroid.runtime.runtime_dir",
                        lambda name: tmp_path / name)
    acct = {"name": "acc0", "base": "arm", "adb_port": 6000,
            "qmp_port": 7000, "vnc_port": 18001}
    qemu_proc.spawn_qemu(acct, {"qemu": {}}, dev=False)
    rj = json.loads((tmp_path / "acc0" / "run.json").read_text())
    assert rj["identity"] == "omni-acc0"
```

- [ ] **Step 2: Run to verify it fails**

Run: `python -m pytest tests/test_verified_liveness.py -q`
Expected: FAIL (`runtime` has no attribute `expected_identity`; run.json has no `identity`).

- [ ] **Step 3: Implement**

In `omnidroid/runtime.py` add:

```python
def expected_identity(rec):
    """The QEMU -name token for this instance: f"omni-{name}". Both
    qemu_command and qemu_command_arm emit exactly this, so it is readable
    back from /proc/<pid>/cmdline and from QMP query-name."""
    return f"omni-{rec['name']}"
```

In `omnidroid/qemu_proc.py`, inside `spawn_qemu`, add `"identity": f"omni-{acct['name']}"` to the `run.json` dict written after `Popen`:

```python
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         "identity": f"omni-{acct['name']}",
         "mode": (mode or {}).get("name", "dev" if dev else DEFAULT_MODE),
         "base": acct["base"],
         "adb_port": acct["adb_port"], "qmp_port": acct["qmp_port"],
         "vnc_port": acct["vnc_port"]}))
```

- [ ] **Step 4: Run to verify it passes**

Run: `python -m pytest tests/test_verified_liveness.py -q`
Expected: PASS.

- [ ] **Step 5: Full suite + facade**

Run: `python -m pytest -q`
Expected: PASS. Then run the facade test (Task 1) — PASS.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/runtime.py omnidroid/qemu_proc.py tests/test_verified_liveness.py
git commit -m "feat: record omni-<name> identity token in run.json"
```

---

### Task 9: Identity probes — cmdline token + QMP `query-name`

**Files:**
- Modify: `omnidroid/runtime.py`
- Test: `tests/test_verified_liveness.py`

**Interfaces:**
- Produces:
  - `runtime._cmdline_has_token(pid, token) -> bool` — Linux `/proc/<pid>/cmdline` contains `token`; always `False` where `/proc` is absent.
  - `runtime._qmp_name(qmp_port, timeout=0.25) -> str | None` — QMP `query-name` result's `name`, or `None` on any error/refusal.

- [ ] **Step 1: Write the failing tests**

```python
# append to tests/test_verified_liveness.py
import socket
import threading

def test_cmdline_has_token_true(tmp_path, monkeypatch):
    # Simulate /proc/<pid>/cmdline as a NUL-joined arg vector containing the token.
    proc_dir = tmp_path / "12345"
    proc_dir.mkdir()
    (proc_dir / "cmdline").write_bytes(b"qemu\x00-name\x00omni-acc0\x00")
    monkeypatch.setattr(runtime, "_PROC", tmp_path)
    assert runtime._cmdline_has_token(12345, "omni-acc0") is True
    assert runtime._cmdline_has_token(12345, "omni-other") is False

def test_cmdline_has_token_no_proc(monkeypatch):
    monkeypatch.setattr(runtime, "_PROC", pathlib.Path("/nonexistent-proc"))
    assert runtime._cmdline_has_token(1, "omni-acc0") is False

def _fake_qmp_server(name):
    # Minimal QMP: greeting, accept qmp_capabilities, answer query-name.
    srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
    port = srv.getsockname()[1]
    def serve():
        c, _ = srv.accept()
        f = c.makefile("rw")
        f.write('{"QMP":{"version":{}}}\n'); f.flush()
        f.readline()                       # qmp_capabilities
        f.write('{"return":{}}\n'); f.flush()
        f.readline()                       # query-name
        f.write(json.dumps({"return": {"name": name}}) + "\n"); f.flush()
        c.close(); srv.close()
    threading.Thread(target=serve, daemon=True).start()
    return port

def test_qmp_name_reads_guest_name():
    port = _fake_qmp_server("omni-acc0")
    assert runtime._qmp_name(port) == "omni-acc0"

def test_qmp_name_none_when_nobody_listens():
    # An almost-certainly-closed port returns None fast.
    assert runtime._qmp_name(1) is None
```

- [ ] **Step 2: Run to verify they fail**

Run: `python -m pytest tests/test_verified_liveness.py -k "cmdline or qmp_name" -q`
Expected: FAIL (`_cmdline_has_token` / `_qmp_name` / `_PROC` undefined).

- [ ] **Step 3: Implement**

In `omnidroid/runtime.py`:

```python
import pathlib

_PROC = pathlib.Path("/proc")   # overridable in tests

def _cmdline_has_token(pid, token):
    """True iff the Linux /proc/<pid>/cmdline arg vector contains `token`.
    Cheap identity confirmation that kills PID-recycle false positives with
    no socket. Returns False anywhere /proc is unavailable (macOS/Windows)."""
    try:
        raw = (_PROC / str(pid) / "cmdline").read_bytes()
    except (OSError, ValueError):
        return False
    return token.encode() in raw.split(b"\x00")

def _qmp_name(qmp_port, timeout=0.25):
    """The guest name from QMP query-name on qmp_port, or None on any
    error/refusal. Fast (short timeout) — runs in hot paths."""
    from omnidroid.qemu_proc import qmp   # lazy: qemu_proc imports runtime_dir
    resp = qmp({"qmp_port": qmp_port}, "query-name", timeout=timeout)
    if not resp:
        return None
    return (resp.get("return") or {}).get("name")
```

- [ ] **Step 4: Run to verify they pass**

Run: `python -m pytest tests/test_verified_liveness.py -k "cmdline or qmp_name" -q`
Expected: PASS.

- [ ] **Step 5: Full suite** — `python -m pytest -q` — Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/runtime.py tests/test_verified_liveness.py
git commit -m "feat: identity probes (cmdline token + QMP query-name)"
```

---

### Task 10: `instance_live()` and wire it into the three call sites

**Files:**
- Modify: `omnidroid/runtime.py` (`running_instances`, `running_pid`, `_claimed_port_indices`)
- Test: `tests/test_verified_liveness.py`

**Interfaces:**
- Consumes: `pid_alive`, `expected_identity`, `_cmdline_has_token`, `_qmp_name`.
- Produces: `runtime.instance_live(rec) -> bool`. `running_instances`, `running_pid`, `_claimed_port_indices` now call `instance_live` instead of bare `pid_alive`.

- [ ] **Step 1: Write the failing tests** (the mode-2 regression lock + identity mismatch)

```python
# append to tests/test_verified_liveness.py
import subprocess

def test_instance_live_rejects_recycled_pid(monkeypatch):
    # A live but UNRELATED process (a sleep). pid is alive, but neither the
    # cmdline token nor QMP identity match -> not our instance.
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 1,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token", lambda pid, tok: False)
        monkeypatch.setattr(runtime, "_qmp_name", lambda port, timeout=0.25: None)
        assert runtime.instance_live(rec) is False
    finally:
        p.terminate(); p.wait()

def test_instance_live_true_on_cmdline_match(monkeypatch):
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 1,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token",
                            lambda pid, tok: tok == "omni-acc0")
        assert runtime.instance_live(rec) is True   # cheap path, no socket
    finally:
        p.terminate(); p.wait()

def test_instance_live_true_on_qmp_match_when_no_cmdline(monkeypatch):
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 5,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token", lambda pid, tok: False)
        monkeypatch.setattr(runtime, "_qmp_name",
                            lambda port, timeout=0.25: "omni-acc0")
        assert runtime.instance_live(rec) is True
    finally:
        p.terminate(); p.wait()

def test_instance_live_false_when_pid_dead(monkeypatch):
    rec = {"name": "acc0", "pid": 999999, "qmp_port": 1, "identity": "omni-acc0"}
    monkeypatch.setattr(runtime, "pid_alive", lambda pid: False)
    assert runtime.instance_live(rec) is False

def test_instance_live_legacy_record_falls_back_to_pid(monkeypatch):
    # No identity field (pre-upgrade run.json) -> trust pid_alive alone.
    rec = {"name": "acc0", "pid": 4242}
    monkeypatch.setattr(runtime, "pid_alive", lambda pid: True)
    assert runtime.instance_live(rec) is True
```

- [ ] **Step 2: Run to verify they fail**

Run: `python -m pytest tests/test_verified_liveness.py -k instance_live -q`
Expected: FAIL (`instance_live` undefined).

- [ ] **Step 3: Implement `instance_live`**

In `omnidroid/runtime.py`:

```python
def instance_live(rec):
    """Verified liveness: is `rec`'s recorded process THE QEMU for this
    instance (not a recycled pid, not a stranger)? Cheap-first.

    1. pid must be alive at all.
    2. Legacy records (no identity) fall back to pid-only with a warning.
    3. Linux cheap path: /proc/<pid>/cmdline carries the -name token -> live.
    4. Authoritative path: QMP query-name equals the token -> live.
    Any ambiguity resolves to NOT live (safe direction: frees nothing that
    is actually answering, and never claims a stranger)."""
    pid = rec.get("pid")
    if not pid_alive(pid):
        return False
    token = rec.get("identity")
    if not token:
        import sys
        sys.stderr.write(
            f"warn: {rec.get('name')} run.json has no identity; "
            f"trusting pid {pid} (pre-upgrade record)\n")
        return True
    if _cmdline_has_token(pid, token):
        return True
    return _qmp_name(rec.get("qmp_port")) == token
```

- [ ] **Step 4: Replace `pid_alive` at the three call sites**

In `running_instances`: change `if pid_alive(data.get("pid")):` to `if instance_live(data):`.
In `running_pid`: change the final `return pid if pid_alive(pid) else None` to:

```python
    return pid if instance_live(data) else None
```

In `_claimed_port_indices`: change `if data.get("adb_port") is not None and pid_alive(data.get("pid")):` to `if data.get("adb_port") is not None and instance_live(data):`.

- [ ] **Step 5: Run to verify all pass**

Run: `python -m pytest tests/test_verified_liveness.py -q`
Expected: PASS.

- [ ] **Step 6: Full suite** — `python -m pytest -q` — Expected: PASS (`tests/test_runtime.py` still green — legacy fallback keeps records without identity working).

- [ ] **Step 7: Commit**

```bash
git add omnidroid/runtime.py tests/test_verified_liveness.py
git commit -m "feat: instance_live() verified liveness replaces pid-only checks"
```

---

### Task 11: Probe-before-allocate safety net

**Files:**
- Modify: `omnidroid/runtime.py` (`allocate_ports`)
- Test: `tests/test_verified_liveness.py`

**Interfaces:**
- Consumes: `_claimed_port_indices`, a new `_port_answers`.
- Produces: `runtime._port_answers(port, timeout=0.25) -> bool`; `allocate_ports` skips any index whose qmp or adb port already answers, even if bookkeeping missed it.

- [ ] **Step 1: Write the failing test**

```python
# append to tests/test_verified_liveness.py
def test_allocate_ports_skips_a_port_that_answers(monkeypatch):
    # No run.json claims anything, but a live listener sits on index 0's qmp
    # port. allocate_ports must NOT hand out index 0.
    srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
    live_port = srv.getsockname()[1]
    cfg = {"qemu": {"adb_port_start": 6000, "qmp_port_start": live_port,
                    "vnc_port_start": 18001}}
    monkeypatch.setattr(runtime, "_claimed_port_indices", lambda: set())
    monkeypatch.setattr(runtime, "vnc_start", lambda c: 18001)
    try:
        adb_port, qmp_port, vnc_port = runtime.allocate_ports(cfg)
        assert qmp_port != live_port          # index 0 was skipped
    finally:
        srv.close()
```

- [ ] **Step 2: Run to verify it fails**

Run: `python -m pytest tests/test_verified_liveness.py -k allocate_ports -q`
Expected: FAIL (index 0 handed out; `_port_answers` undefined).

- [ ] **Step 3: Implement**

In `omnidroid/runtime.py` add:

```python
import socket as _socket

def _port_answers(port, timeout=0.25):
    """True iff something accepts a TCP connection on 127.0.0.1:port. Final
    collision guard: never issue a port a live QEMU answers on. A refused
    connection means free; any other socket error resolves to True (treat as
    occupied) — ambiguity costs one port index, never a collision."""
    try:
        with _socket.create_connection(("127.0.0.1", port), timeout=timeout):
            return True
    except ConnectionRefusedError:
        return False          # nobody home -> free
    except OSError:
        return True           # ambiguous -> treat as occupied (safe direction)
```

Then in `allocate_ports`, after computing `used` and before returning, change the index search so a candidate whose qmp OR adb port answers is also skipped:

```python
    used = {p - q["adb_port_start"] for p in _claimed_port_indices()}
    i = 0
    while True:
        if i in used:
            i += 1
            continue
        adb_port = q["adb_port_start"] + i
        qmp_port = q["qmp_port_start"] + i
        if _port_answers(qmp_port) or _port_answers(adb_port):
            i += 1
            continue
        return (adb_port, qmp_port, vnc_start(cfg) + i)
```

- [ ] **Step 4: Run to verify it passes**

Run: `python -m pytest tests/test_verified_liveness.py -k allocate_ports -q`
Expected: PASS.

- [ ] **Step 5: Full suite** — `python -m pytest -q` — Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/runtime.py tests/test_verified_liveness.py
git commit -m "feat: probe-before-allocate — never issue a port a live QEMU answers on"
```

---

### Task 12: `reconcile_runtime()` self-heal sweep

**Files:**
- Modify: `omnidroid/runtime.py`
- Test: `tests/test_verified_liveness.py`

**Interfaces:**
- Consumes: `instance_live`, `_port_answers`, `config.data_dir`, `_wipe_runtime`.
- Produces: `runtime.reconcile_runtime() -> dict` with keys `{"gc": [names], "orphans": [ports]}`; GCs dirs whose pid is dead and port silent, reports (does not adopt) live QEMUs with no run.json.

- [ ] **Step 1: Write the failing tests**

```python
# append to tests/test_verified_liveness.py
def test_reconcile_gcs_dead_and_silent(tmp_path, monkeypatch):
    root = tmp_path / "runtime"; (root / "acc0").mkdir(parents=True)
    (root / "acc0" / "run.json").write_text(json.dumps(
        {"pid": 999999, "identity": "omni-acc0", "qmp_port": 1,
         "adb_port": 2}))
    monkeypatch.setattr(runtime.config, "data_dir", lambda: tmp_path)
    monkeypatch.setattr(runtime, "instance_live", lambda rec: False)
    monkeypatch.setattr(runtime, "_port_answers", lambda port, timeout=0.25: False)
    result = runtime.reconcile_runtime()
    assert "acc0" in result["gc"]
    assert not (root / "acc0").exists()

def test_reconcile_keeps_live_instance(tmp_path, monkeypatch):
    root = tmp_path / "runtime"; (root / "acc0").mkdir(parents=True)
    (root / "acc0" / "run.json").write_text(json.dumps(
        {"pid": 4242, "identity": "omni-acc0", "qmp_port": 1, "adb_port": 2}))
    monkeypatch.setattr(runtime.config, "data_dir", lambda: tmp_path)
    monkeypatch.setattr(runtime, "instance_live", lambda rec: True)
    result = runtime.reconcile_runtime()
    assert result["gc"] == []
    assert (root / "acc0").exists()
```

- [ ] **Step 2: Run to verify they fail**

Run: `python -m pytest tests/test_verified_liveness.py -k reconcile -q`
Expected: FAIL (`reconcile_runtime` undefined).

- [ ] **Step 3: Implement**

In `omnidroid/runtime.py`:

```python
def reconcile_runtime():
    """Sweep runtime/*: GC directories whose recorded process is dead AND whose
    ports are silent; report (do NOT adopt) any run.json-less directory. Returns
    {"gc": [names], "orphans": [names]}. Synchronous — callers trigger it."""
    result = {"gc": [], "orphans": []}
    root = config.data_dir() / "runtime"
    if not root.exists():
        return result
    for d in sorted(root.iterdir()):
        rj = d / "run.json"
        if not rj.exists():
            result["orphans"].append(d.name)
            continue
        try:
            data = json.loads(rj.read_text())
        except Exception:  # noqa: BLE001
            continue
        if data.get("reserving"):
            continue
        if instance_live(data):
            continue
        qmp_port = data.get("qmp_port")
        adb_port = data.get("adb_port")
        silent = not ((qmp_port and _port_answers(qmp_port))
                      or (adb_port and _port_answers(adb_port)))
        if silent:
            _wipe_runtime(d.name)
            result["gc"].append(d.name)
    return result
```

- [ ] **Step 4: Run to verify they pass**

Run: `python -m pytest tests/test_verified_liveness.py -k reconcile -q`
Expected: PASS.

- [ ] **Step 5: Wire the sweep into `cmd_list` and `cmd_start`**

In `omnidroid/engine.py`, at the top of `cmd_list(args)` (before it reads instances) and at the top of `cmd_start(args)` (before `allocate_ports`), add:

```python
    from omnidroid.runtime import reconcile_runtime
    reconcile_runtime()
```

(No output change: reconcile is silent bookkeeping; `cmd_list` then lists the reconciled state.)

- [ ] **Step 6: Full suite + facade** — `python -m pytest -q` then the facade test — Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add omnidroid/runtime.py omnidroid/engine.py tests/test_verified_liveness.py
git commit -m "feat: reconcile_runtime() self-heal sweep wired into list/start"
```

---

## Follow-up (out of scope for this plan)

The remaining Thread B extractions — `imaging.py` (disk/partition surgery + base
building) and `roblox.py` (session/cookie/login/kiosk delivery) — are a separate
plan. They are independent of verified-liveness and can land any time after this
plan. Threads C (density/scale) and D (daemon+observability) are future specs.
