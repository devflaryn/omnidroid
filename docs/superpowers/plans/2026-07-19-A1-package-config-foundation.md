# A1 — Package & Config Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the OmniDroid manager from a single `manager/omni.py` (6517 lines) into an installable `omnidroid/` package with a `manager.py` shim, and centralize all cross-platform path/arch logic in one `config.py` — **with zero behavior change**. This is the clean base that A2 (diskless model) and A3 (logging/cleanup) build on.

**Architecture:** Move the four existing modules (`omni.py`, `cookies.py`, `capture.py`, `vncview.py`) into an `omnidroid/` package. Extract the path/platform primitives that are currently module-level globals in `omni.py` into `omnidroid/config.py`, adding macOS defaults, `OMNI_IMAGES_DIR`/`OMNI_DATA_DIR` env overrides, and a formal `data_dir()`. Expose three equivalent invocation routes (`omnidroid` console script, `python -m omnidroid`, `python manager.py`) all dispatching to `omnidroid.engine.main`. The existing pytest suite is the regression safety net — it must stay green throughout.

**Tech Stack:** Python 3.13+, argparse CLI, pytest, `pyproject.toml` (setuptools) for the console-script entry point.

## Global Constraints

- **Zero behavior change.** Every existing command (`create`, `start`, `stop`, `list`, `login`, `accounts`, `session`, `doctor`, etc.) behaves byte-identically after A1. Removing commands and the folder model is A2, not A1.
- **The existing pytest suite must pass after every task** — `tests/test_cookies.py`, `test_session.py`, `test_dev_gate.py`, `test_ephemeral_boot.py`, `test_install_recovery.py`, `test_keyframe_thresholds.py`, `test_pixel_format.py`. These are the safety net for the move.
- **Module names kept in A1** where a rename adds risk: `cookies.py` stays `cookies.py` (its rename to `accounts.py` happens in A2 when its schema is extended — the natural moment). Only `omni.py` is renamed, to `engine.py`.
- **Python floor: 3.13** (host runs cpython 3.13/3.14).
- **Cross-platform:** Windows, macOS, Linux must all resolve paths correctly. `IS_WINDOWS`/`IS_MACOS`/`IS_LINUX`/`IS_ARM64_HOST` semantics are preserved exactly.
- **Frozen-exe support preserved:** `_app_root()`'s PyInstaller (`sys.frozen`) branch must keep working — the shipped product is a frozen exe.
- **Secrets:** `accounts.json` (cookie store) stays mode `0600` and gitignored. Never print/log a cookie.
- **Run tests with:** `python -m pytest tests/ -q` from the repo root (`/Users/berat/Desktop/Omni Apps/omnidroid`).

---

## File Structure After A1

```
manager.py                 # NEW root shim -> omnidroid.cli:main (also keeps `python manager.py` working)
pyproject.toml             # NEW package metadata + console_scripts: omnidroid = omnidroid.cli:main
omnidroid/                 # NEW package
  __init__.py              # NEW package marker + version
  __main__.py              # NEW `python -m omnidroid`
  cli.py                   # NEW thin entry -> omnidroid.engine.main
  config.py                # NEW extracted path/platform/arch primitives (+ macOS, env overrides, data_dir)
  engine.py                # MOVED from manager/omni.py (imports rewired to package + config.py)
  cookies.py               # MOVED from manager/cookies.py (unchanged content)
  capture.py               # MOVED from manager/capture.py (import vncview -> package-relative)
  vncview.py               # MOVED from manager/vncview.py (unchanged content)
manager/                   # DELETED at end of A1 (empty after moves)
tests/                     # import paths updated: manager -> omnidroid
```

**Responsibilities:**
- `config.py` — the *only* place that knows platform branches, the repo root, the images dir, the data dir, and how to resolve a QEMU binary. Everything else imports from it.
- `engine.py` — all runtime + build command logic (the old `omni.py` body), temporarily still large; A2/A3 carve it down.
- `cli.py` / `__main__.py` / `manager.py` — three thin entry points, no logic.

---

### Task 1: Package skeleton + `pyproject.toml`

Create the empty package and its metadata so `import omnidroid` works and the console script is declared. Nothing moves yet.

**Files:**
- Create: `omnidroid/__init__.py`
- Create: `pyproject.toml`
- Test: `tests/test_package_skeleton.py`

**Interfaces:**
- Produces: importable package `omnidroid` with `omnidroid.__version__: str`.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_package_skeleton.py
import importlib


def test_omnidroid_package_imports():
    mod = importlib.import_module("omnidroid")
    assert isinstance(mod.__version__, str)
    assert mod.__version__
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_package_skeleton.py -q`
Expected: FAIL with `ModuleNotFoundError: No module named 'omnidroid'`

- [ ] **Step 3: Create the package marker**

```python
# omnidroid/__init__.py
"""OmniDroid — multi-account Android (Roblox) instance manager."""

__version__ = "0.1.0"
```

- [ ] **Step 4: Create `pyproject.toml`**

```toml
# pyproject.toml
[build-system]
requires = ["setuptools>=68"]
build-backend = "setuptools.build_meta"

[project]
name = "omnidroid"
version = "0.1.0"
description = "Multi-account Android (Roblox) instance manager"
requires-python = ">=3.13"
dependencies = []

[project.scripts]
omnidroid = "omnidroid.cli:main"

[tool.setuptools]
py-modules = ["manager"]

[tool.setuptools.packages.find]
include = ["omnidroid*"]
```

- [ ] **Step 5: Run test to verify it passes**

Run: `python -m pytest tests/test_package_skeleton.py -q`
Expected: PASS (run from repo root so `omnidroid/` is importable via cwd).

- [ ] **Step 6: Commit**

```bash
git add omnidroid/__init__.py pyproject.toml tests/test_package_skeleton.py
git commit -m "feat(pkg): add omnidroid package skeleton + pyproject console-script"
```

---

### Task 2: Move the stable peer modules into the package

Move `cookies.py`, `capture.py`, `vncview.py` into `omnidroid/` unchanged except `capture.py`'s `import vncview`. Update their tests. These three have no dependency on `omni.py`, so they move cleanly first.

**Files:**
- Move: `manager/cookies.py` -> `omnidroid/cookies.py`
- Move: `manager/capture.py` -> `omnidroid/capture.py`
- Move: `manager/vncview.py` -> `omnidroid/vncview.py`
- Modify: `omnidroid/capture.py:45` (`import vncview` -> package-relative)
- Modify: `tests/test_cookies.py:19-22`, `tests/test_pixel_format.py:17-18`, `tests/test_keyframe_thresholds.py:16-17`

**Interfaces:**
- Produces: `omnidroid.cookies`, `omnidroid.capture`, `omnidroid.vncview` importable; public APIs unchanged (`cookies.accounts_path`, `save_account`, `get_account`, `list_accounts`, `whoami`, etc.).

- [ ] **Step 1: Move the three files with git**

```bash
git mv manager/cookies.py omnidroid/cookies.py
git mv manager/capture.py omnidroid/capture.py
git mv manager/vncview.py omnidroid/vncview.py
```

- [ ] **Step 2: Fix `capture.py`'s peer import**

In `omnidroid/capture.py`, change line 45 from:

```python
import vncview
```

to:

```python
from omnidroid import vncview
```

- [ ] **Step 3: Update the three tests' import paths**

In `tests/test_cookies.py`, replace lines 19-22:

```python
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                os.pardir, "manager"))

import cookies  # noqa: E402
```

with:

```python
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import cookies  # noqa: E402
```

In `tests/test_pixel_format.py`, replace lines 17-18:

```python
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                os.pardir, "manager"))
```

with:

```python
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
```

and change any bare `import vncview` / `import capture` in that file to `from omnidroid import vncview` / `from omnidroid import capture`.

In `tests/test_keyframe_thresholds.py`, apply the same two changes as `test_pixel_format.py` (lines 16-17 sys.path, and the `capture`/`vncview` imports).

- [ ] **Step 4: Run the moved-module tests to verify they pass**

Run: `python -m pytest tests/test_cookies.py tests/test_pixel_format.py tests/test_keyframe_thresholds.py -q`
Expected: PASS (same assertions as before, new import path).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(pkg): move cookies/capture/vncview into omnidroid package"
```

---

### Task 3: Move `omni.py` -> `omnidroid/engine.py` and rewire imports

Move the 6517-line module into the package as `engine.py`, fix its peer imports and `_app_root()` depth, and keep it runnable. This is the big mechanical move; verification is "the CLI runs and the omni-importing tests pass." No logic changes.

**Files:**
- Move: `manager/omni.py` -> `omnidroid/engine.py`
- Modify: `omnidroid/engine.py` — peer imports (`import cookies` -> `from omnidroid import cookies`; same for any `import capture` / `import vncview`), `_app_root()` (line 34-40), `prog="omni"` -> `prog="omnidroid"` (line 5958)
- Modify: `tests/test_session.py:20-24`, `tests/test_dev_gate.py:15-18`, `tests/test_ephemeral_boot.py:11-12`, `tests/test_install_recovery.py:15-16`

**Interfaces:**
- Consumes: `omnidroid.cookies`, `omnidroid.capture`, `omnidroid.vncview` (from Task 2).
- Produces: `omnidroid.engine` importable; `omnidroid.engine.main()` is the argparse dispatcher; all `cmd_*` functions and module globals (`REPO`, `ACCOUNTS_DIR`, `QEMU_DIR`, `IS_WINDOWS`, ...) live here until Task 4 extracts the path ones.

- [ ] **Step 1: Move the file**

```bash
git mv manager/omni.py omnidroid/engine.py
```

- [ ] **Step 2: Fix peer imports inside `engine.py`**

Find every top-level peer import (there are `import cookies as _ck` calls *inside functions* too — e.g. `cmd_accounts`, `account_cookie`, `_capture_and_save_account`). Replace each occurrence:

- `import cookies as _ck` -> `from omnidroid import cookies as _ck`
- `import cookies` -> `from omnidroid import cookies`
- `import capture` -> `from omnidroid import capture`
- `import vncview` -> `from omnidroid import vncview`

Run this to find them all first:

```bash
grep -n "import cookies\|import capture\|import vncview" omnidroid/engine.py
```

Edit each hit (module-level and in-function) to the `from omnidroid import ...` form.

- [ ] **Step 3: Fix `_app_root()` for the new package depth**

`engine.py` now sits at `omnidroid/engine.py`, one level deeper than `manager/omni.py` was — but both had the repo root as `parent.parent`, so the depth is unchanged. Verify `_app_root()` (lines 34-40) still reads:

```python
def _app_root():
    if getattr(sys, "frozen", False):
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent.parent
```

`omnidroid/engine.py` -> `.parent` = `omnidroid/`, `.parent.parent` = repo root. Correct — leave as-is. (This step is a deliberate verification, not a no-op: confirm the repo root still resolves to `/Users/berat/Desktop/Omni Apps/omnidroid`.)

- [ ] **Step 4: Change the argparse prog name**

In `engine.py`, line ~5958, change:

```python
    p = argparse.ArgumentParser(prog="omni")
```

to:

```python
    p = argparse.ArgumentParser(prog="omnidroid")
```

- [ ] **Step 5: Update the four omni-importing tests**

In each of `tests/test_session.py`, `tests/test_dev_gate.py`, `tests/test_ephemeral_boot.py`, `tests/test_install_recovery.py`:

Replace the `sys.path.insert(... "manager")` line with:

```python
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
```

Replace `import omni` with `from omnidroid import engine as omni` (aliasing to `omni` keeps the rest of each test file unchanged). In `test_session.py` also replace `import cookies as ck` with `from omnidroid import cookies as ck`.

- [ ] **Step 6: Run the full suite to verify no regression**

Run: `python -m pytest tests/ -q`
Expected: PASS (all 8 test files, including `test_package_skeleton.py`).

- [ ] **Step 7: Smoke-test the CLI directly**

Run: `python -c "from omnidroid import engine; import sys; sys.argv=['omnidroid','version']; engine.main()"`
Expected: prints the version JSON/text exactly as `python manager/omni.py version` did before the move (no traceback).

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor(pkg): move omni.py -> omnidroid/engine.py, rewire imports"
```

---

### Task 4: Extract path/platform primitives into `config.py`

Move the platform/path/arch module-level globals and resolvers out of `engine.py` into `omnidroid/config.py`; `engine.py` imports them back. Pure relocation — same values, same behavior.

**Files:**
- Create: `omnidroid/config.py`
- Modify: `omnidroid/engine.py` (delete the moved globals/functions, add `from omnidroid.config import ...`)
- Test: `tests/test_config.py`

**Interfaces:**
- Produces (in `omnidroid/config.py`):
  - Constants: `REPO: Path`, `CONFIG_PATH: Path`, `ACCOUNTS_DIR: Path`, `QEMU_DIR: Path`, `IS_WINDOWS: bool`, `IS_LINUX: bool`, `IS_MACOS: bool`, `HOST_ARCH: str`, `IS_ARM64_HOST: bool`.
  - Functions moved verbatim: `_app_root()`, `resolve_images_dir(cfg)`, `qemu_bin(tool)`, `qemu_system_name()`.
- Consumes: nothing from engine (this is the leaf module).

- [ ] **Step 1: Write the failing test (locks the contract)**

```python
# tests/test_config.py
import os
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import config  # noqa: E402


def test_repo_root_is_project_dir():
    assert (config.REPO / "omnidroid" / "engine.py").exists()


def test_platform_flags_are_mutually_consistent():
    assert sum([config.IS_WINDOWS, config.IS_LINUX, config.IS_MACOS]) == 1


def test_qemu_system_name_matches_host():
    name = config.qemu_system_name()
    assert name in ("qemu-system-aarch64", "qemu-system-x86_64")


def test_resolve_images_dir_expands_and_absolutizes():
    got = config.resolve_images_dir({"images_dir": "~/OmniImages"})
    assert Path(got).is_absolute()
    assert "~" not in got
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_config.py -q`
Expected: FAIL with `ImportError` / `ModuleNotFoundError: omnidroid.config`.

- [ ] **Step 3: Create `config.py` with the moved primitives**

Cut these from `engine.py` and paste into `omnidroid/config.py` **unchanged**: the `_app_root()` function (lines 34-40) and the constants block `REPO … IS_ARM64_HOST` (lines 43-54); `resolve_images_dir()` (lines 355-373); `qemu_bin()` (lines 561-580) and `qemu_system_name()` (lines 583-587). `config.py` needs these imports at top:

```python
# omnidroid/config.py
"""Single source of truth for platform, paths, and QEMU-binary resolution.

Every path/arch decision in OmniDroid routes through here so one checkout runs
on Windows, macOS, and Linux without any other module knowing the branches.
"""
import platform
import sys
from pathlib import Path


def _app_root():
    """Project root — works as a .py and as a PyInstaller onefile exe."""
    if getattr(sys, "frozen", False):
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent.parent


REPO = _app_root()
CONFIG_PATH = REPO / "configs" / "paths.json"
ACCOUNTS_DIR = REPO / "accounts"
QEMU_DIR = REPO / "qemu"
IS_WINDOWS = platform.system() == "Windows"
IS_LINUX = platform.system() == "Linux"
IS_MACOS = platform.system() == "Darwin"
HOST_ARCH = platform.machine().lower()
IS_ARM64_HOST = HOST_ARCH in ("arm64", "aarch64")
```

Then paste `resolve_images_dir`, `qemu_bin`, `qemu_system_name` below. **Important:** `qemu_bin` calls `read_config()` (which stays in `engine.py`). To avoid a circular import, change `qemu_bin` to accept the config dict via a lazy import: replace its `qd = read_config().get("qemu", {}).get("dir")` line with:

```python
    try:
        from omnidroid import engine
        qd = engine.read_config().get("qemu", {}).get("dir")
    except Exception:
        qd = None
```

(The lazy `from omnidroid import engine` inside the function body breaks the import cycle — `config` stays importable standalone, which the test relies on.)

- [ ] **Step 4: Point `engine.py` at `config.py`**

In `engine.py`, delete the now-moved definitions (the constants block, `_app_root`, `resolve_images_dir`, `qemu_bin`, `qemu_system_name`) and add near the top of `engine.py` (after the stdlib imports):

```python
from omnidroid.config import (
    REPO, CONFIG_PATH, ACCOUNTS_DIR, QEMU_DIR,
    IS_WINDOWS, IS_LINUX, IS_MACOS, HOST_ARCH, IS_ARM64_HOST,
    resolve_images_dir, qemu_bin, qemu_system_name,
)
```

- [ ] **Step 5: Run the full suite to verify no regression**

Run: `python -m pytest tests/ -q`
Expected: PASS (config test + all prior tests; engine still works via the re-imported names).

- [ ] **Step 6: Smoke-test a base-touching command**

Run: `python -c "from omnidroid import engine; import sys; sys.argv=['omnidroid','doctor']; engine.main()"`
Expected: same doctor output as before extraction (path/qemu resolution intact).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(config): extract path/platform/qemu primitives into config.py"
```

---

### Task 5: Cross-platform additions — macOS default, env overrides, `data_dir()`

Add the A1 cross-platform deliverables to `config.py`: an explicit macOS images default, `OMNI_IMAGES_DIR` / `OMNI_DATA_DIR` env overrides, and a formal `data_dir()`. Wire `engine.py`'s data locations (the cookie-store `accounts.json` and `ACCOUNTS_DIR`) through `data_dir()` so A2 can relocate them by env without touching engine.

**Files:**
- Modify: `omnidroid/config.py` (add `data_dir()`, env overrides, macOS default)
- Modify: `omnidroid/engine.py` (use `config.data_dir()` for the store path passed to `cookies.*`, and derive `ACCOUNTS_DIR` from it)
- Test: `tests/test_config.py` (extend)

**Interfaces:**
- Produces:
  - `config.data_dir() -> Path` — dir holding `accounts.json`, `accounts/`, and (A3) `logs/`, `runtime/`. Default = `REPO`; override `OMNI_DATA_DIR` (expanded, created if missing).
  - `config.images_dir(cfg) -> str` — wraps `resolve_images_dir`, but `OMNI_IMAGES_DIR` env wins when set.
  - macOS branch in `resolve_images_dir` returns `~/OmniImages` (already present via the `darwin` key/linux fallback — make it explicit).

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_config.py`:

```python
def test_data_dir_defaults_to_repo(monkeypatch):
    monkeypatch.delenv("OMNI_DATA_DIR", raising=False)
    assert config.data_dir() == config.REPO


def test_data_dir_env_override(tmp_path, monkeypatch):
    target = tmp_path / "omnidata"
    monkeypatch.setenv("OMNI_DATA_DIR", str(target))
    got = config.data_dir()
    assert got == target
    assert got.exists()  # created on demand


def test_images_dir_env_override(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_IMAGES_DIR", str(tmp_path / "imgs"))
    got = config.images_dir({"images_dir": "~/OmniImages"})
    assert Path(got) == (tmp_path / "imgs")


def test_images_dir_no_env_uses_config(tmp_path, monkeypatch):
    monkeypatch.delenv("OMNI_IMAGES_DIR", raising=False)
    got = config.images_dir({"images_dir": str(tmp_path / "cfgimgs")})
    assert Path(got) == (tmp_path / "cfgimgs")
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python -m pytest tests/test_config.py -q`
Expected: FAIL with `AttributeError: module 'omnidroid.config' has no attribute 'data_dir'`.

- [ ] **Step 3: Add the new functions to `config.py`**

Add after the constants block:

```python
import os


def data_dir() -> Path:
    """Directory holding accounts.json, accounts/, logs/, runtime/.
    Defaults to the project root; OMNI_DATA_DIR relocates it (created if
    missing) so state can travel independently of the code checkout."""
    env = os.environ.get("OMNI_DATA_DIR")
    if env:
        p = Path(env).expanduser()
        p.mkdir(parents=True, exist_ok=True)
        return p
    return REPO


def images_dir(cfg) -> str:
    """Absolute images dir. OMNI_IMAGES_DIR wins over the config value so a
    host can point at an external image store without editing paths.json."""
    env = os.environ.get("OMNI_IMAGES_DIR")
    if env:
        return str(Path(env).expanduser())
    return resolve_images_dir(cfg)
```

Make the macOS branch in `resolve_images_dir` explicit — change the platform-key line so `darwin` maps to the linux-style `~/OmniImages` default when a config provides no `darwin` key (behavior already exists; this is a readability edit, keep the existing fallback semantics).

- [ ] **Step 4: Wire `engine.py` data paths through `data_dir()`**

In `engine.py`, the cookie store is addressed as `cookies.accounts_path(REPO)` / `_ck.list_accounts(REPO)` etc. (search `REPO)` calls into `cookies`). Introduce a helper and use it at those call sites:

```python
def _store_root():
    from omnidroid import config
    return config.data_dir()
```

Replace `REPO` with `_store_root()` in the `cookies.*` calls (`account_cookie`, `cmd_accounts`, `_capture_and_save_account`, and any other `_ck.<fn>(REPO ...)`). Also set `ACCOUNTS_DIR = config.data_dir() / "accounts"` (import-time) instead of the `config.REPO / "accounts"` constant, so folder accounts follow `OMNI_DATA_DIR` too. Default behavior is identical (data_dir() == REPO when the env is unset).

- [ ] **Step 5: Run the full suite**

Run: `python -m pytest tests/ -q`
Expected: PASS.

- [ ] **Step 6: Verify env override end-to-end**

Run:

```bash
OMNI_DATA_DIR="$CLAUDE_JOB_DIR/tmp/omnidata-test" python -c "from omnidroid import config; print(config.data_dir())"
```

Expected: prints `.../omnidata-test` and the dir now exists (`ls "$CLAUDE_JOB_DIR/tmp/omnidata-test"`).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(config): macOS default, OMNI_IMAGES_DIR/OMNI_DATA_DIR overrides, data_dir()"
```

---

### Task 6: Three invocation routes — `cli.py`, `__main__.py`, `manager.py`

Add the thin entry points so `omnidroid`, `python -m omnidroid`, and `python manager.py` all dispatch to the same `engine.main`.

**Files:**
- Create: `omnidroid/cli.py`
- Create: `omnidroid/__main__.py`
- Create: `manager.py` (repo root)
- Test: `tests/test_entrypoints.py`

**Interfaces:**
- Consumes: `omnidroid.engine.main`.
- Produces: `omnidroid.cli.main` (the console-script target declared in `pyproject.toml` Task 1).

- [ ] **Step 1: Write the failing test**

```python
# tests/test_entrypoints.py
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _run(args):
    return subprocess.run([sys.executable, *args], cwd=ROOT,
                          capture_output=True, text=True, timeout=60)


def test_module_route():
    r = _run(["-m", "omnidroid", "version"])
    assert r.returncode == 0, r.stderr


def test_manager_shim_route():
    r = _run(["manager.py", "version"])
    assert r.returncode == 0, r.stderr


def test_cli_main_is_callable():
    from omnidroid import cli
    assert callable(cli.main)
```

(Add the standard `sys.path.insert(0, ROOT)` for the `from omnidroid import cli` import.)

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_entrypoints.py -q`
Expected: FAIL (`No module named omnidroid.__main__` / `omnidroid.cli`).

- [ ] **Step 3: Create `cli.py`**

```python
# omnidroid/cli.py
"""Console entry point. All command logic lives in omnidroid.engine."""
from omnidroid.engine import main


def main_entry():
    main()


# pyproject's console_scripts points at `omnidroid.cli:main`.
__all__ = ["main"]
```

- [ ] **Step 4: Create `__main__.py`**

```python
# omnidroid/__main__.py
from omnidroid.cli import main

if __name__ == "__main__":
    main()
```

- [ ] **Step 5: Create the root `manager.py` shim**

```python
#!/usr/bin/env python3
"""Thin shim so `python manager.py <cmd>` keeps working without install."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from omnidroid.cli import main

if __name__ == "__main__":
    main()
```

- [ ] **Step 6: Run the entry-point tests**

Run: `python -m pytest tests/test_entrypoints.py -q`
Expected: PASS (both subprocess routes exit 0; `cli.main` callable).

- [ ] **Step 7: Run the full suite**

Run: `python -m pytest tests/ -q`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(cli): manager.py shim + python -m omnidroid + cli entry"
```

---

### Task 7: Installable console command + delete empty `manager/`

Verify `pip install -e .` yields a working `omnidroid` command, and remove the now-empty `manager/` directory.

**Files:**
- Delete: `manager/` (empty after Task 3 moved its last file)
- Modify: `.gitignore` if needed (confirm `accounts.json` still ignored)

**Interfaces:** none new.

- [ ] **Step 1: Confirm `manager/` is empty and remove it**

```bash
ls manager/ 2>/dev/null   # expect: only __pycache__ (or nothing)
rm -rf manager
```

- [ ] **Step 2: Editable-install the package**

```bash
python -m pip install -e .
```

Expected: installs `omnidroid` with no errors; writes an `omnidroid` console script.

- [ ] **Step 3: Verify the console command dispatches**

Run: `omnidroid version`
Expected: same version output as `python -m omnidroid version` — exit 0, no traceback.

- [ ] **Step 4: Verify a store command works via the console script**

Run: `omnidroid accounts --json`
Expected: valid JSON listing the (currently 3) saved accounts, `ok: true`, no cookie fields — identical to the pre-refactor output.

- [ ] **Step 5: Full regression suite one more time**

Run: `python -m pytest tests/ -q`
Expected: PASS (all files).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore(pkg): remove empty manager/, confirm editable install + console script"
```

---

## Verification (whole-plan, run after Task 7)

Behavior-preservation is the whole point of A1. Confirm each route and a representative command:

1. `python -m pytest tests/ -q` → all green.
2. `omnidroid version`, `python -m omnidroid version`, `python manager.py version` → identical output, all exit 0.
3. `omnidroid accounts --json` → same account list as before A1, no cookie leakage.
4. `omnidroid doctor` → same readiness report (path/qemu resolution intact on this macOS/arm64 host).
5. `OMNI_DATA_DIR=/tmp/omni-x omnidroid accounts --json` → reads/writes the store under `/tmp/omni-x` (proves `data_dir()` relocation) then clean up.
6. `git status` → clean; `manager/` gone; new files: `manager.py`, `pyproject.toml`, `omnidroid/*`.

If any command's output differs from its pre-A1 behavior (beyond the `prog` name in `--help`), that is a regression — fix before declaring A1 done.

---

## Self-Review notes (author)

- **Spec coverage:** A1 covers the spec's section 4 (refactor/layout: `manager.py` shim, `omnidroid/` package, three routes, importable API) and section 5 (cross-platform: `config.py` owns paths, macOS added, `OMNI_IMAGES_DIR`/`OMNI_DATA_DIR`, `data_dir()`, QEMU-bin resolution centralized). Spec sections 1-3, 6-7 (diskless model, logging, CDN seam, migration) are **deferred to A2/A3 by design** — not gaps.
- **Deferred deliberately:** `cookies.py` -> `accounts.py` rename lands in A2 (when the schema is extended); command removal (`create`/`play`/`resume`) and folder deletion are A2; `logs/`, `runtime/`, retention, `baseimg.py` split, `test.apk` deletion are A3.
- **Type consistency:** `config.data_dir()`/`config.images_dir(cfg)` names are used identically in Tasks 5-7. `omnidroid.engine.main` is the single dispatcher referenced by `cli.py`, `__main__.py`, `manager.py`.
- **Risk note:** Task 3 is the one big-bang move; its safety net is the pre-existing pytest suite plus the CLI smoke test. If `grep` in Step 2 reveals a peer import this plan didn't anticipate, apply the same `from omnidroid import ...` rewrite.
