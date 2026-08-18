# omni-qemu Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the product ship a QEMU we build from a patch series this repo owns, so the gaming window resizes without flicker, its X asks before it acts, and a farming guest's RAM stops costing Windows commit charge.

**Architecture:** Seven numbered patches against a pinned QEMU tag, applied by `tools/build-qemu.py`, built for `x86_64-softmmu` and `aarch64-softmmu`, staged into the same portable bundle shape the product already downloads. Five patches already exist as an unsplit diff at `qemu-patches/0000-omni-all-WIP.patch`; two are new and both are Windows memory work. The Python side then stops working around what the binary could not do: the external aspect-lock process is deleted rather than tuned, `window-close=off` comes off, and guest RAM moves onto a mapped sparse file.

**Tech Stack:** QEMU 11.1.0, C (mingw-w64 gcc 15.2 via MSYS2 at `C:\msys64`), meson/ninja, Python 3.13 stdlib only, `unittest` + pytest as the runner.

**Spec:** `docs/superpowers/specs/2026-08-19-omni-qemu-and-density-design.md`

## Global Constraints

- **No third-party Python dependencies.** `pyproject.toml` declares `dependencies = []`. The strip is stdlib + `ctypes`. Do not add a package.
- **Never `git add -A`, `git checkout`, `git stash`, or `git reset` in this repo.** It carries ~60 uncommitted tracked files of pre-existing WIP. Every commit step names its files explicitly. `bases.py:120-123` carries an annotation about a constant already lost to a stray `git checkout` once.
- **QEMU source tree:** `C:\qemubuild`, currently QEMU 11.1.0 (`v11.1.0-dirty`) with the five omni patches applied as uncommitted edits. Upstream pin for the series is tag **`v11.1.0`**, commit `84f07211cc`.
- **The configure line that produced the working build** (recovered from `build/config.status`):
  `--enable-gtk --enable-opengl --enable-virglrenderer --enable-slirp --enable-whpx --disable-docs --disable-werror`
  with `--target-list=` set per task.
- **Build toolchain:** `export PATH="/c/msys64/mingw64/bin:$PATH"` before any `gcc`/`meson`/`ninja`. Already installed: gcc 15.2.0, glib2 2.88.3, gtk3 3.24.52, meson 1.12.0, ninja 1.13.2, pixman 0.46.4.
- **Test runner.** There is no pytest config in `pyproject.toml`. The working invocation is documented at `docs/HANDOFF-WINDOWS.md:2691-2696`:
  ```bash
  cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
  OMNIDROID_CONFIG_PATH=/tmp/test-paths.json OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
    python -m pytest tests/ -q
  ```
  Two traps: run it with a config that **has a base registered** or collection dies with `INTERNALERROR ... SystemExit: no base image is registered` (`tests/test_qemu_accepts_devices.py` calls `load_config()` at import); and **copy the config fresh each run**, because the engine writes back to it. Baseline is **11 failed / 932 passed**; diff the `FAILED` lines against that baseline, never count them.
- **`tests/engine_public_names.json` is a facade contract** enforced by `tests/test_facade_equivalence.py`. Removing a public `engine.<name>` requires removing it from that JSON in the same commit.
- **Verify the frozen build, not the source.** The engine is frozen in from a sibling checkout at build time, so "the source is fixed" and "the shipped exe is fixed" are different claims.
- **Do not re-open a probabilistic failure on one lucky run.** PS99 at `-m 2048` survived once in five.
- **`_windowlock` is deleted by THIS plan** (Task 8). The parallel plan `2026-08-19-D-remove-tkinter-viewer.md` deletes `_vncview`/`_windowbar` and must not touch `_windowlock`, `hostwin.py`, or `qemu_proc.py`.

---

### Task 1: Split the WIP diff into a numbered patch series

**Files:**
- Create: `qemu-patches/0001-omni-window-icon.patch`
- Create: `qemu-patches/0002-omni-aspect-lock.patch`
- Create: `qemu-patches/0003-omni-panel-pin.patch`
- Create: `qemu-patches/0004-omni-confirm-close.patch`
- Create: `qemu-patches/0005-omni-win32-discard.patch`
- Create: `qemu-patches/0006-omni-win32-build-no-symlinks.patch`
- Create: `qemu-patches/SERIES` (ordered list, one filename per line)
- Create: `qemu-patches/PIN` (one line: `v11.1.0`)
- Delete: `qemu-patches/0000-omni-all-WIP.patch`
- Test: `tests/test_qemu_patch_series.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `qemu-patches/SERIES` — newline-separated patch filenames in apply order, `#` comments and blank lines ignored. `qemu-patches/PIN` — a single upstream git tag. Task 2's `tools/build_qemu.read_series()` reads both.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_qemu_patch_series.py
"""The QEMU patch series is versioned, ordered, and complete.

This file exists because the series spent months as UNCOMMITTED edits in
C:\\qemubuild, a directory in no repo. The test does not compile anything --
it asserts the series is a series: every file named in SERIES exists, every
patch file on disk is named in SERIES, the order is the numeric order, and
the pin is a single tag.
"""
import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PATCHES = REPO / "qemu-patches"


def read_series():
    lines = (PATCHES / "SERIES").read_text(encoding="utf-8").splitlines()
    return [ln.strip() for ln in lines
            if ln.strip() and not ln.strip().startswith("#")]


class SeriesShape(unittest.TestCase):
    def test_every_named_patch_exists(self):
        for name in read_series():
            self.assertTrue((PATCHES / name).is_file(),
                            f"SERIES names {name}, which is not on disk")

    def test_every_patch_on_disk_is_named(self):
        on_disk = sorted(p.name for p in PATCHES.glob("*.patch"))
        self.assertEqual(on_disk, sorted(read_series()),
                         "a patch exists that SERIES does not apply")

    def test_series_is_in_numeric_order(self):
        nums = [int(re.match(r"(\d+)-", n).group(1)) for n in read_series()]
        self.assertEqual(nums, sorted(nums))
        self.assertEqual(len(set(nums)), len(nums), "duplicate patch number")

    def test_the_wip_snapshot_is_gone(self):
        self.assertFalse((PATCHES / "0000-omni-all-WIP.patch").exists(),
                         "the unsplit snapshot is superseded by the series")

    def test_pin_is_one_tag(self):
        pin = (PATCHES / "PIN").read_text(encoding="utf-8").strip()
        self.assertRegex(pin, r"^v\d+\.\d+\.\d+$")


class SeriesContent(unittest.TestCase):
    """Each patch touches the files the design says it touches, and no others."""

    EXPECTED = {
        "0001-omni-window-icon.patch": {"ui/gtk.c"},
        "0002-omni-aspect-lock.patch": {"ui/gtk.c", "include/ui/gtk.h",
                                        "ui/gtk-gl-area.c"},
        "0003-omni-panel-pin.patch": {"ui/gtk.c"},
        "0004-omni-confirm-close.patch": {"ui/gtk.c"},
        "0005-omni-win32-discard.patch": {"system/physmem.c"},
        "0006-omni-win32-build-no-symlinks.patch":
            {"scripts/symlink-install-tree.py"},
    }

    def test_each_patch_touches_only_its_files(self):
        for name, expected in self.EXPECTED.items():
            text = (PATCHES / name).read_text(encoding="utf-8")
            touched = set(re.findall(r"^\+\+\+ b/(.+)$", text, re.M))
            self.assertEqual(touched, expected, f"{name} touches {touched}")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: FAIL — `SERIES` does not exist (`FileNotFoundError`).

- [ ] **Step 3: Split the WIP diff by hand**

The source of truth is the live tree. Work there, do not hand-edit the snapshot:

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd /c/qemubuild
git diff --stat        # expect: include/ui/gtk.h, scripts/symlink-install-tree.py,
                       # system/physmem.c, ui/gtk-gl-area.c, ui/gtk.c
```

`scripts/symlink-install-tree.py` **is** an omni patch and becomes `0006`. It was nearly written off as scaffolding; it is not. Meson's bundle step calls `os.symlink`, Windows refuses symlinks without Developer Mode or administrator rights (`WinError 1314`), and stock QEMU's answer is to print *"Please enable Developer Mode to support soft link"* and fail the build. The patch skips the bundle tree on Windows instead, which costs nothing here because we install to a prefix rather than running QEMU out of the build directory — and a copy is explicitly **not** a substitute, because meson points those links at files the build has not produced yet.

**Two things to check while lifting it.** The live edit has a mangled continuation on the `if os.name == 'nt' and isinstance(e, OSError) and e.errno != errno.EEXIST:` line — write it as a well-formed multi-line condition. And confirm `errno` is imported at the top of the file; if the stock script imports only `os` and `sys`, the patch must add the import, or the build dies with `NameError` at the first non-`EEXIST` error instead of skipping it.

Produce each patch with an explicit path list and `git diff -- <paths>`, then hand-split `ui/gtk.c` (which carries four of the five) by hunk. The hunk boundaries are unambiguous because each block is prefixed `/* omni: ... */`:

| patch | `ui/gtk.c` region | anchor |
|---|---|---|
| 0001 | `gd_create_menus`/init tail | `QEMU_WINDOW_ICON` (~line 3016) |
| 0002 | `omni_guest_size`, `omni_aspect_filter`, `omni_fit_window_to_aspect`, `omni_install_aspect_filter`, the `QEMU_WINDOW_LOCK_ASPECT` branch of `gd_update_geometry_hints`, and the `omni_fit_window_to_aspect` call (~line 531) | `WM_SIZING` |
| 0003 | the `QEMU_WINDOW_LOCK_ASPECT` + `QEMU_WINDOW_PANEL` block in `gd_set_ui_size` | `QEMU_WINDOW_PANEL` |
| 0004 | the `QEMU_WINDOW_CONFIRM_CLOSE` block in `gd_window_close` (~line 919) | `gtk_message_dialog_new` |

`include/ui/gtk.h` and `ui/gtk-gl-area.c` both belong to 0002 (they declare and call `omni_fit_window_to_aspect`).

Write each as a plain unified diff with `--- a/<path>` / `+++ b/<path>` headers so `git apply` takes it.

- [ ] **Step 4: Write SERIES and PIN**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid/qemu-patches"
cat > SERIES <<'EOF'
# Applied in this order against the tag in PIN. See
# docs/superpowers/specs/2026-08-19-omni-qemu-and-density-design.md §3a.
0001-omni-window-icon.patch
0002-omni-aspect-lock.patch
0003-omni-panel-pin.patch
0004-omni-confirm-close.patch
0005-omni-win32-discard.patch
0006-omni-win32-build-no-symlinks.patch
EOF
echo v11.1.0 > PIN
rm 0000-omni-all-WIP.patch
```

- [ ] **Step 5: Verify the series applies cleanly to the pinned tag**

This is the check that matters and it is not a unit test — it needs the source tree.

```bash
cd /c/qemubuild
git stash list                      # MUST be empty; if not, STOP and report
git worktree add /c/qemu-verify v11.1.0
cd /c/qemu-verify
for p in "/c/Users/berat/Desktop/Omni Apps/omnidroid/qemu-patches"/0*.patch; do
  git apply --check "$p" && git apply "$p" && echo "OK $(basename "$p")" || \
    { echo "FAILED $(basename "$p")"; break; }
done
git diff --stat                     # must equal the original 320-line stat,
                                     # minus scripts/symlink-install-tree.py
cd /c && git -C /c/qemubuild worktree remove --force /c/qemu-verify
```

Expected: six `OK` lines, and a diff stat equal to the original 320-insertion
stat — `include/ui/gtk.h | 3 +`, `scripts/symlink-install-tree.py | 8 +`,
`system/physmem.c | 35 +`, `ui/gtk-gl-area.c | 7 +`, `ui/gtk.c | 269 +`. A
smaller total means a hunk was dropped in the split, which is the one failure
mode of this task that a green test suite will not catch.

- [ ] **Step 6: Run the test to verify it passes**

```bash
python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: PASS, 6 tests.

- [ ] **Step 7: Commit**

```bash
git add qemu-patches/SERIES qemu-patches/PIN \
        qemu-patches/0001-omni-window-icon.patch \
        qemu-patches/0002-omni-aspect-lock.patch \
        qemu-patches/0003-omni-panel-pin.patch \
        qemu-patches/0004-omni-confirm-close.patch \
        qemu-patches/0005-omni-win32-discard.patch \
        qemu-patches/0006-omni-win32-build-no-symlinks.patch \
        tests/test_qemu_patch_series.py
git rm qemu-patches/0000-omni-all-WIP.patch
git commit -m "qemu: split the rescued diff into a numbered, verified series

Six patches, applied in SERIES order against the tag in PIN, verified to apply
cleanly to a pristine v11.1.0 worktree. The unsplit snapshot is gone.

The sixth was nearly discarded as scaffolding. It is not: meson's bundle step
symlinks, Windows refuses symlinks without Developer Mode, and stock QEMU's
answer is to fail the build telling you to go and enable it. Skipping the
bundle tree costs nothing here because we install to a prefix -- but only
someone who had run the build would know that, which is exactly why it
belongs in the series instead of in a working directory.

tests/test_qemu_patch_series.py asserts the series is a series -- nothing on
disk unapplied, nothing named that is missing, numeric order, one pin -- and
that each patch touches only the files the design assigns it."
```

---

### Task 2: `tools/build-qemu.py` — apply, configure, build, stage

**Files:**
- Create: `tools/build_qemu.py` (importable module, underscore name)
- Create: `tools/build-qemu.py` (thin CLI shim: `from tools.build_qemu import main`)
- Test: `tests/test_build_qemu.py`

**Interfaces:**
- Consumes: `qemu-patches/SERIES`, `qemu-patches/PIN` from Task 1.
- Produces:
  - `read_series(patches_dir: Path) -> list[Path]`
  - `read_pin(patches_dir: Path) -> str`
  - `configure_argv(prefix: Path, targets: list[str], extra: list[str] = ()) -> list[str]`
  - `apply_argv(patch: Path) -> list[str]`
  - `verify_applied(work: Path, series: list[Path]) -> list[str]`
  - `stage_plan(build_dir: Path, out_dir: Path, targets: list[str]) -> list[tuple[Path, Path]]`
  - `main(argv: list[str] | None = None) -> int`

  Task 9 calls `configure_argv` and `stage_plan`.

**Why `verify_applied` exists — read this before writing it.** During Task 1's
fix round, growing patch `0001` by nine lines caused `git apply` to place
`0003`'s panel-pin hunk inside `gd_set_ui_refresh_rate` instead of
`gd_set_ui_size` — **and print `OK`**. Byte count right, location wrong, exit
status zero. `git apply` matches on surrounding context, so any change to a
patch's size, any rebase onto a newer upstream tag, and any build on a machine
whose tree differs slightly can slide a hunk into a neighbouring function and
report success. The result compiles, runs, and is wrong. A diff-stat comparison
cannot see it — the byte count was correct. Only an anchor check can, and the
build script is where it belongs, because the build script is what applies the
series on every machine including the Mac.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_build_qemu.py
"""The QEMU build is a script, not a shell history.

Everything here is pure: argv construction and plan shape. Nothing compiles.
The build itself is verified by running it (Task 2 Step 5), because a build
is not a thing a unit test can pin.
"""
import tempfile
import unittest
from pathlib import Path

from tools.build_qemu import (apply_argv, configure_argv, read_pin,
                              read_series, stage_plan, verify_applied)

REPO = Path(__file__).resolve().parent.parent
PATCHES = REPO / "qemu-patches"


class Series(unittest.TestCase):
    def test_reads_in_order_and_skips_comments(self):
        names = [p.name for p in read_series(PATCHES)]
        self.assertEqual(names[0], "0001-omni-window-icon.patch")
        # Deliberately not asserting names[-1]: the series GROWS in tasks 3
        # and 4, and a test that has to be edited every time a patch is added
        # is a test that gets edited without being read.
        self.assertEqual(names, sorted(names))
        self.assertNotIn("SERIES", names)
        self.assertNotIn("PIN", names)

    def test_pin_is_the_tag(self):
        self.assertEqual(read_pin(PATCHES), "v11.1.0")


class Configure(unittest.TestCase):
    def test_carries_the_flags_that_produced_the_working_build(self):
        argv = configure_argv(Path("/out"), ["x86_64-softmmu"])
        for flag in ("--enable-gtk", "--enable-opengl",
                     "--enable-virglrenderer", "--enable-slirp",
                     "--enable-whpx", "--disable-docs", "--disable-werror"):
            self.assertIn(flag, argv)

    def test_targets_are_one_comma_joined_flag(self):
        argv = configure_argv(Path("/out"),
                              ["x86_64-softmmu", "aarch64-softmmu"])
        self.assertIn("--target-list=x86_64-softmmu,aarch64-softmmu", argv)

    def test_whpx_is_dropped_off_windows(self):
        # --enable-whpx on a non-Windows host fails configure outright.
        argv = configure_argv(Path("/out"), ["aarch64-softmmu"],
                              host_os="darwin")
        self.assertNotIn("--enable-whpx", argv)
        self.assertIn("--enable-hvf", argv)

    def test_prefix_is_absolute_and_first(self):
        argv = configure_argv(Path("/out/pfx"), ["x86_64-softmmu"])
        self.assertTrue(argv[0].endswith("configure"))
        self.assertIn("--prefix=/out/pfx", [a.replace("\\", "/") for a in argv])


class Apply(unittest.TestCase):
    def test_checks_before_it_applies(self):
        # A patch that will not apply must not half-apply. `git apply` is
        # atomic per invocation, so the contract is simply: one invocation.
        argv = apply_argv(Path("/p/0001.patch"))
        self.assertEqual(argv[:2], ["git", "apply"])
        self.assertIn("--check", apply_argv(Path("/p/0001.patch"), check=True))


class AnchorCheck(unittest.TestCase):
    """A hunk that lands in the wrong function still applies, still compiles,
    and still reports OK. This happened for real in Task 1's fix round --
    0003's panel-pin block went into gd_set_ui_refresh_rate instead of
    gd_set_ui_size when 0001 grew by nine lines. Diff stats cannot see it.
    """

    GOOD = """
static void gd_set_ui_refresh_rate(VirtualConsole *vc, int refresh_rate)
{
    QemuUIInfo info;
    info.refresh_rate = refresh_rate;
}

static void gd_set_ui_size(VirtualConsole *vc, gint width, gint height)
{
    if (g_getenv("QEMU_WINDOW_LOCK_ASPECT")) {
        const char *panel = g_getenv("QEMU_WINDOW_PANEL");
    }
}
"""
    BAD = """
static void gd_set_ui_refresh_rate(VirtualConsole *vc, int refresh_rate)
{
    QemuUIInfo info;
    const char *panel = g_getenv("QEMU_WINDOW_PANEL");
}

static void gd_set_ui_size(VirtualConsole *vc, gint width, gint height)
{
    return;
}
"""

    def _tree(self, body):
        d = Path(tempfile.mkdtemp())
        (d / "ui").mkdir()
        (d / "ui" / "gtk.c").write_text(body, encoding="utf-8")
        (d / "system").mkdir()
        (d / "system" / "physmem.c").write_text("", encoding="utf-8")
        return d

    def test_clean_tree_reports_no_violations(self):
        out = verify_applied(self._tree(self.GOOD),
                             [Path("0003-omni-panel-pin.patch")])
        self.assertEqual([v for v in out if "QEMU_WINDOW_PANEL" in v], [])

    def test_misplaced_hunk_is_caught(self):
        out = verify_applied(self._tree(self.BAD),
                             [Path("0003-omni-panel-pin.patch")])
        self.assertTrue(any("QEMU_WINDOW_PANEL" in v and "gd_set_ui_size" in v
                            for v in out),
                        f"a hunk in the wrong function went unreported: {out}")

    def test_anchors_for_absent_patches_are_not_asserted(self):
        """0007 and 0008 do not exist until tasks 3 and 4. Their anchors must
        not fail a build that legitimately has not got them yet."""
        out = verify_applied(self._tree(self.GOOD), [])
        self.assertEqual(out, [])


class Stage(unittest.TestCase):
    def test_stages_only_the_emulators_the_engine_invokes(self):
        plan = stage_plan(Path("/b"), Path("/out"),
                          ["x86_64-softmmu", "aarch64-softmmu"])
        names = sorted(dst.name for _, dst in plan)
        self.assertIn("qemu-system-x86_64.exe", names)
        self.assertIn("qemu-system-aarch64.exe", names)
        self.assertIn("qemu-img.exe", names)
        # ~58 system emulators exist; three ship. grep qemu_bin( in engine.py.
        self.assertNotIn("qemu-system-alpha.exe", names)

    def test_share_is_staged_wholesale(self):
        plan = stage_plan(Path("/b"), Path("/out"), ["x86_64-softmmu"])
        self.assertTrue(any(str(src).endswith("pc-bios") for src, _ in plan),
                        "firmware is loaded lazily and BY NAME -- an "
                        "allow-list boots here and fails on a customer's")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_build_qemu.py -q
```
Expected: FAIL — `ModuleNotFoundError: No module named 'tools.build_qemu'`.

- [ ] **Step 3: Write the module**

```python
# tools/build_qemu.py
"""Build the product's QEMU from qemu-patches/ against the pinned tag.

WHY THIS EXISTS. The five patches this applies spent months as uncommitted
edits in a directory that was in no repo, built once, for one architecture,
by hand. That is not a build; it is a machine that happens to have the right
file on it. Everything below is the difference.

WHAT IT DELIBERATELY DOES NOT DO. It does not prune with an allow-list.
QEMU loads option ROMs lazily and BY NAME (`efi-virtio.rom` per NIC,
`vgabios-*.bin` per display model, `kvmvapic.bin`, `linuxboot_dma.bin`), so
"the firmware an x86 guest needs" is a list that boots on the machine that
wrote it and fails on a customer's six weeks later. share/ is staged whole
and only what cannot possibly apply is dropped -- docs, desktop icons, and
the other-architecture UEFI blobs, which is where the size is anyway.
"""
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PATCHES_DIR = REPO / "qemu-patches"

# The three emulators the engine actually invokes. `grep qemu_bin(` in
# engine.py before changing this list.
STAGED_TOOLS = ("qemu-img", "qemu-nbd", "qemu-edid")

_TARGET_BINARY = {
    "x86_64-softmmu": "qemu-system-x86_64",
    "aarch64-softmmu": "qemu-system-aarch64",
    "i386-softmmu": "qemu-system-i386",
}

# Flags recovered from the config.status of the build that worked
# (C:\qemubuild, 2026-08-16). Do not "tidy" them.
_COMMON_FLAGS = (
    "--enable-gtk",
    "--enable-opengl",
    "--enable-virglrenderer",
    "--enable-slirp",
    "--disable-docs",
    "--disable-werror",
)

_ACCEL_FLAG = {"win32": "--enable-whpx", "darwin": "--enable-hvf",
               "linux": "--enable-kvm"}


def read_series(patches_dir: Path = PATCHES_DIR) -> list[Path]:
    """Patch paths in apply order. `#` comments and blank lines are ignored."""
    text = (patches_dir / "SERIES").read_text(encoding="utf-8")
    out = []
    for line in text.splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            out.append(patches_dir / line)
    return out


def read_pin(patches_dir: Path = PATCHES_DIR) -> str:
    """The upstream git tag the series is written against."""
    return (patches_dir / "PIN").read_text(encoding="utf-8").strip()


def _host_key(host_os: str | None = None) -> str:
    plat = host_os or sys.platform
    if plat.startswith("win"):
        return "win32"
    if plat == "darwin":
        return "darwin"
    return "linux"


def configure_argv(prefix: Path, targets: list[str], extra=(),
                   host_os: str | None = None, source: Path | None = None):
    """argv for QEMU's ./configure.

    The accelerator flag is host-selected because `--enable-whpx` on a
    non-Windows host does not warn, it FAILS configure -- which on the Mac
    would read as "the patch series is broken".
    """
    cfg = (source or Path(".")) / "configure"
    argv = [str(cfg), f"--prefix={prefix.as_posix()}",
            "--target-list=" + ",".join(targets)]
    argv.extend(_COMMON_FLAGS)
    argv.append(_ACCEL_FLAG[_host_key(host_os)])
    argv.extend(extra)
    return argv


def apply_argv(patch: Path, check: bool = False) -> list[str]:
    """argv for applying one patch. One invocation, so it is atomic."""
    argv = ["git", "apply"]
    if check:
        argv.append("--check")
    argv.append(str(patch))
    return argv


# Each omni symbol, and the function whose body it MUST sit inside. See
# verify_applied() for why this table exists rather than a diff-stat check.
# The third element is the patch that introduces the symbol; anchors whose
# patch is not in the series being applied are skipped, so this table can
# name 0007/0008 before they exist.
_ANCHORS = (
    ("ui/gtk.c", "QEMU_WINDOW_TITLE", "gd_update_caption", "0001"),
    ("ui/gtk.c", "omni_install_aspect_filter", "gd_update_geometry_hints",
     "0002"),
    ("ui/gtk.c", "QEMU_WINDOW_PANEL", "gd_set_ui_size", "0003"),
    ("ui/gtk.c", "QEMU_WINDOW_CONFIRM_CLOSE", "gd_window_close", "0004"),
    ("system/physmem.c", "DiscardVirtualMemory", "ram_block_discard_range",
     "0005"),
    ("system/physmem.c", "omni_win32_file_ram_alloc", "ram_block_add", "0007"),
    ("system/physmem.c", "FSCTL_SET_ZERO_DATA", "ram_block_discard_range",
     "0008"),
)


def _enclosing_function(text: str, needle: str):
    """Name of the C function whose body contains the first `needle`.

    Brace-counting from the top of the file rather than a regex, because the
    thing being guarded against is a symbol landing in the WRONG function --
    and a regex that searches backwards for the nearest `foo(...)` finds a
    call site as readily as a definition. Returns None if `needle` is absent
    or sits at file scope.
    """
    idx = text.find(needle)
    if idx < 0:
        return None
    depth = 0
    current = None
    pending = None
    for pos, ch in enumerate(text):
        if pos >= idx:
            break
        if ch == "\n":
            pending = None
        elif ch == "(" and depth == 0:
            # remember the identifier immediately before this paren
            j = pos
            while j > 0 and (text[j - 1].isalnum() or text[j - 1] == "_"):
                j -= 1
            pending = text[j:pos] or None
        elif ch == "{":
            if depth == 0:
                current = pending
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                current = None
    return current


def verify_applied(work: Path, series):
    """Anchor check: did every omni hunk land inside the function it belongs
    to? Returns a list of human-readable violations, empty when clean.

    THIS IS NOT BELT AND BRACES. In Task 1's fix round, growing patch 0001 by
    nine lines made `git apply` place 0003's panel-pin block inside
    gd_set_ui_refresh_rate instead of gd_set_ui_size -- and exit 0. The byte
    count was right; only the location was wrong, so a diff-stat comparison
    reported success. `git apply` matches on surrounding context, so every
    rebase onto a newer tag and every build on a slightly different tree can
    reproduce it. The result compiles, runs, and is wrong.
    """
    have = {p.name[:4] for p in series}
    out = []
    for rel, symbol, want_fn, patch_num in _ANCHORS:
        if patch_num not in have:
            continue                      # that patch is not in this series
        path = work / rel
        if not path.is_file():
            out.append(f"{rel}: missing, cannot check {symbol}")
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        if symbol not in text:
            out.append(f"{rel}: {symbol} not found -- patch {patch_num} "
                       f"did not apply, or applied somewhere unexpected")
            continue
        got = _enclosing_function(text, symbol)
        if got != want_fn:
            out.append(f"{rel}: {symbol} is inside {got!r}, expected "
                       f"{want_fn!r} -- patch {patch_num} landed in the "
                       f"wrong function and `git apply` did not say so")
    return out


def stage_plan(build_dir: Path, out_dir: Path, targets: list[str]):
    """(src, dst) pairs for the portable bundle. Pure -- touches no disk."""
    exe = ".exe" if _host_key() == "win32" else ""
    plan: list[tuple[Path, Path]] = []
    for t in targets:
        name = _TARGET_BINARY[t] + exe
        plan.append((build_dir / name, out_dir / name))
    for tool in STAGED_TOOLS:
        plan.append((build_dir / f"{tool}{exe}", out_dir / f"{tool}{exe}"))
    # Firmware, wholesale. See the module docstring for why this is not
    # an allow-list.
    plan.append((build_dir / "pc-bios", out_dir / "share"))
    return plan


def _run(argv, cwd=None, env=None):
    print("+", " ".join(str(a) for a in argv), flush=True)
    subprocess.run(argv, cwd=cwd, env=env, check=True)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--source", required=True, type=Path,
                    help="a QEMU git checkout; a worktree at PIN is made here")
    ap.add_argument("--out", required=True, type=Path,
                    help="staging directory for the portable bundle")
    ap.add_argument("--targets", default="x86_64-softmmu,aarch64-softmmu")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--skip-build", action="store_true",
                    help="apply and configure only")
    a = ap.parse_args(argv)

    targets = [t for t in a.targets.split(",") if t]
    pin = read_pin()
    work = a.source.parent / f"qemu-omni-{pin}"

    if not work.exists():
        _run(["git", "-C", str(a.source), "worktree", "add",
              str(work), pin])

    series = read_series()
    for patch in series:
        _run(apply_argv(patch, check=True), cwd=work)
        _run(apply_argv(patch), cwd=work)

    # `git apply` printing OK is not evidence the hunks went where they
    # belong. See verify_applied().
    violations = verify_applied(work, series)
    if violations:
        for v in violations:
            print(f"ANCHOR: {v}", file=sys.stderr)
        raise SystemExit("patches applied to the wrong place; refusing to build")

    build = work / "build"
    build.mkdir(exist_ok=True)
    _run(configure_argv(a.out, targets, source=work), cwd=build)
    if a.skip_build:
        return 0
    _run(["ninja", f"-j{a.jobs}"], cwd=build)

    a.out.mkdir(parents=True, exist_ok=True)
    for src, dst in stage_plan(build, a.out, targets):
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        else:
            shutil.copy2(src, dst)
        print(f"staged {dst}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

Then the CLI shim:

```python
# tools/build-qemu.py
"""Hyphenated entry point; the module is tools/build_qemu.py."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from tools.build_qemu import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
```

And make `tools/` importable:

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
touch tools/__init__.py
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
python -m pytest tests/test_build_qemu.py -q
```
Expected: PASS, 12 tests.

- [ ] **Step 4b: Prove the anchor check bites on the real series**

A guard that has never fired is a guard nobody has tested. Apply the series to
a throwaway worktree, deliberately break one patch's context, and confirm the
build refuses:

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
# 1. clean series -> no violations
python -c "
from pathlib import Path
from tools.build_qemu import read_series, verify_applied
import subprocess
subprocess.run(['git','-C','/c/qemubuild','worktree','add','/c/qemu-anchor','v11.1.0'],check=True)
for p in read_series():
    subprocess.run(['git','apply',str(p)],cwd='/c/qemu-anchor',check=True)
print('violations:', verify_applied(Path('/c/qemu-anchor'), read_series()))
"
# expect: violations: []
# 2. move one omni block into the neighbouring function BY HAND in
#    /c/qemu-anchor/ui/gtk.c (cut the QEMU_WINDOW_PANEL block out of
#    gd_set_ui_size and paste it into gd_set_ui_refresh_rate), re-run the
#    verify_applied call above, and confirm it now reports the violation
#    naming both functions.
git -C /c/qemubuild worktree remove --force /c/qemu-anchor
```

Expected: `[]` first, then a violation string containing `QEMU_WINDOW_PANEL`,
`gd_set_ui_refresh_rate` and `gd_set_ui_size`. If step 2 still reports `[]`,
`_enclosing_function` is wrong and the guard is decorative — fix it before
moving on.

- [ ] **Step 5: Run the real build — x86_64 and aarch64 together**

This is the first time `aarch64-softmmu` has been built from this series; the
existing tree is `TARGET_DIRS=x86_64-softmmu` only.

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
python tools/build-qemu.py --source /c/qemubuild --out /c/qemu-omni-staged \
       --targets x86_64-softmmu,aarch64-softmmu
```

Expected: exit 0, and

```bash
/c/qemu-omni-staged/qemu-system-x86_64.exe --version   # 11.1.0
/c/qemu-omni-staged/qemu-system-aarch64.exe --version  # 11.1.0
/c/qemu-omni-staged/qemu-system-x86_64.exe -accel help # tcg, whpx
strings /c/qemu-omni-staged/qemu-system-x86_64.exe | grep QEMU_WINDOW_
# QEMU_WINDOW_ICON / QEMU_WINDOW_LOCK_ASPECT / QEMU_WINDOW_PANEL /
# QEMU_WINDOW_CONFIRM_CLOSE  -- the shipped 11.0.50 carries NONE of these
```

If the aarch64 target fails to configure or link, record the exact error and
stop — it is a real finding, not a step to work around. Do not fall back to
x86-only silently; the ARM base has no patched binary today and that is one
of the two gaps this task exists to close.

- [ ] **Step 6: Commit**

```bash
git add tools/__init__.py tools/build_qemu.py tools/build-qemu.py \
        tests/test_build_qemu.py
git commit -m "qemu: a build script, and the first aarch64 build of the series

Applies qemu-patches/SERIES to a worktree at PIN, configures with the exact
flags recovered from the config.status of the build that worked, and stages
the three emulators the engine invokes plus share/ wholesale.

share/ is a deny-list on purpose. QEMU loads option ROMs lazily and by name,
so an allow-list of 'the firmware an x86 guest needs' boots on the machine
that wrote it and fails on a customer's six weeks later.

--enable-whpx is host-selected: on a non-Windows host it does not warn, it
fails configure, which on the Mac would read as a broken patch series."
```

---

### Task 3: Patch 0007 — guest RAM from a mapped file on Windows

**Files:**
- Create: `qemu-patches/0007-omni-win32-ram-file.patch`
- Modify: `qemu-patches/SERIES`
- Test: `tests/test_qemu_patch_series.py` (extend `SeriesContent.EXPECTED`)

**Interfaces:**
- Consumes: `read_series` from Task 2.
- Produces: a QEMU that, when `QEMU_RAM_FILE_DIR` is set in its environment, backs every anonymous guest RAM block with a sparse file in that directory and records the file's Win32 `HANDLE` in a new `void *omni_ram_file` field on `RAMBlock` (`include/system/ramblock.h`), NULL when unused. Task 4 reads that handle. Task 6 sets the env var.

**Why a `void *` and not an `int` fd — this is not style.** RAMBlocks are
`g_malloc0`'d (`physmem.c:2354`, `:2527`), so a new `int` field arrives as
**0** — and 0 is a valid file descriptor, so `>= 0` would be true for every
RAM block in QEMU, including every one with no omni file behind it. Task 4
would then fire `FSCTL_SET_ZERO_DATA` at stdin on every discard. QEMU knows
this trap and guards it by hand: it writes `new_block->fd = -1` and
`->guest_memfd = -1` immediately after each `g_malloc0` (`:2532-2533`), and
the two allocation sites are not symmetric about it. A pointer makes the
sentinel bug unrepresentable — `g_malloc0` gives NULL — and keeps the CRT fd
table out of the path entirely.

**Why this shape and not `-object memory-backend-file`.** `backends/hostmem-file.c`
is excluded on Windows at `backends/meson.build:13` and `file_ram_alloc` sits
under `#if defined(CONFIG_POSIX)` at `physmem.c:1542`. Porting that object
type means QAPI, meson, and the whole fd-based `qemu_ram_alloc_from_fd` path.
Backing the *anonymous* allocation instead is one function and one call site,
needs no new object, no QAPI, and no command-line change — and it covers every
RAM block rather than only the one the machine type wires to a backend.

- [ ] **Step 1: Write the failing test**

Extend the existing series test rather than adding a file:

```python
# tests/test_qemu_patch_series.py -- in class SeriesContent
    EXPECTED = {
        "0001-omni-window-icon.patch": {"ui/gtk.c"},
        "0002-omni-aspect-lock.patch": {"ui/gtk.c", "include/ui/gtk.h",
                                        "ui/gtk-gl-area.c"},
        "0003-omni-panel-pin.patch": {"ui/gtk.c"},
        "0004-omni-confirm-close.patch": {"ui/gtk.c"},
        "0005-omni-win32-discard.patch": {"system/physmem.c"},
        "0007-omni-win32-ram-file.patch": {"system/physmem.c"},
    }

    def test_ram_file_patch_is_env_gated(self):
        """A build that ships this must behave exactly like stock QEMU until
        the launcher opts in. `omnidroid` is not the only thing that will ever
        run this binary."""
        text = (PATCHES / "0007-omni-win32-ram-file.patch").read_text(
            encoding="utf-8")
        self.assertIn("QEMU_RAM_FILE_DIR", text)
        self.assertIn("FILE_ATTRIBUTE_TEMPORARY", text)
        self.assertIn("FSCTL_SET_SPARSE", text)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: FAIL — `0007-omni-win32-ram-file.patch` is not on disk, so
`test_every_patch_on_disk_is_named` still passes but `EXPECTED` lookup raises
`FileNotFoundError`.

- [ ] **Step 3: Write the C**

In the worktree, add to `system/physmem.c`. Place it immediately above
`ram_block_add` (which is at line 2149 in the pinned tree):

```c
#ifdef _WIN32
/* omni: back guest RAM with a mapped sparse FILE instead of private commit.
 *
 * WHY. Windows charges system commit for every privately committed page, and
 * QEMU allocates the whole of `-m` that way -- so 30 farming instances need
 * ~92-120 GB of commit, i.e. a ~100 GB pagefile, on a 32 GB host. A mapped
 * view of a file is backed by THAT FILE, not by the pagefile, and is not
 * charged.
 *
 * MEASURED on the target host before this was written (tools/probes/):
 *
 *   3 GiB of guest RAM                       system commit
 *   VirtualAlloc(MEM_RESERVE|MEM_COMMIT)         +3078 MB
 *   CreateFileMapping + MapViewOfFile               +12 MB
 *
 *   WHvMapGpaRange(file-backed)          -> 0x00000000  OK
 *   FSCTL_SET_ZERO_DATA while GPA-mapped -> 1 (err 0), readback 0x00
 *
 * That second block is the one that decided this: WHPX accepts the mapping
 * AND does not pin it, so a discard is a real discard (see patch 0008).
 *
 * OFF unless QEMU_RAM_FILE_DIR is set. A stock invocation must be bit-for-bit
 * stock.
 */
static void *omni_win32_file_ram_alloc(RAMBlock *block, size_t size,
                                       Error **errp)
{
    const char *dir = g_getenv("QEMU_RAM_FILE_DIR");
    g_autofree char *path = NULL;
    HANDLE fh, mh;
    DWORD junk = 0;
    void *addr;

    if (!dir || !*dir) {
        return NULL;                    /* stock path */
    }

    path = g_strdup_printf("%s%comni-ram-%lu-%p.bin", dir, G_DIR_SEPARATOR,
                           (unsigned long)GetCurrentProcessId(), (void *)block);

    /* DELETE_ON_CLOSE so a killed QEMU cannot leave the file behind, and
     * TEMPORARY so the cache manager does not eagerly write it out.
     * FILE_SHARE_* is deliberately wide: the reaper in qemu_proc.py walks
     * this directory while instances are live. */
    fh = CreateFileA(path, GENERIC_READ | GENERIC_WRITE,
                     FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                     NULL, CREATE_ALWAYS,
                     FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE,
                     NULL);
    if (fh == INVALID_HANDLE_VALUE) {
        error_setg(errp, "omni: cannot create RAM file %s (%lu)",
                   path, GetLastError());
        return NULL;
    }
    /* Sparse, so EndOfFile is the guest's -m while AllocationSize tracks only
     * what the guest has actually touched -- and so patch 0008 can punch it
     * back down. Without this the file is fully allocated on first write and
     * the disk cost equals -m, which trades one wall for another. */
    if (!DeviceIoControl(fh, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL)) {
        error_setg(errp, "omni: RAM file is not sparse (%lu)", GetLastError());
        CloseHandle(fh);
        return NULL;
    }

    mh = CreateFileMappingA(fh, NULL, PAGE_READWRITE,
                            (DWORD)((uint64_t)size >> 32),
                            (DWORD)((uint64_t)size & 0xFFFFFFFFu), NULL);
    if (!mh) {
        error_setg(errp, "omni: CreateFileMapping %zu failed (%lu)",
                   size, GetLastError());
        CloseHandle(fh);
        return NULL;
    }
    addr = MapViewOfFile(mh, FILE_MAP_ALL_ACCESS, 0, 0, size);
    /* The section keeps the mapping alive; the handle is not needed again. */
    CloseHandle(mh);
    if (!addr) {
        error_setg(errp, "omni: MapViewOfFile %zu failed (%lu)",
                   size, GetLastError());
        CloseHandle(fh);
        return NULL;
    }

    /* Keep the HANDLE itself. NOT an int fd: RAMBlock is g_malloc0'd, so a
     * new int field would arrive as 0 -- a VALID descriptor -- and every
     * block in QEMU would look like it had one. NULL is the natural zero. */
    block->omni_ram_file = fh;

    return addr;
}
#endif /* _WIN32 */
```

Then the call site, inside `ram_block_add`, replacing the `else` arm at
`physmem.c:2171-2183`:

```c
        } else {
#ifdef _WIN32
            Error *omni_err = NULL;
            new_block->host = omni_win32_file_ram_alloc(
                new_block, new_block->max_length, &omni_err);
            if (omni_err) {
                /* Opting in and then silently falling back to private commit
                 * would look like the feature working and reproduce exactly
                 * the pagefile this exists to remove. Fail loudly. */
                error_propagate(errp, omni_err);
                qemu_mutex_unlock_ramlist();
                return;
            }
            if (!new_block->host)
#endif
            new_block->host = qemu_anon_ram_alloc(new_block->max_length,
                                                  &new_block->mr->align,
                                                  shared, noreserve);
            if (!new_block->host) {
```

Add near the top of `system/physmem.c`, with the other `#ifdef _WIN32`
includes:

```c
#ifdef _WIN32
#include <io.h>          /* _open_osfhandle */
#include <winioctl.h>    /* FSCTL_SET_SPARSE */
#endif
```

- [ ] **Step 4: Build it and prove the numbers**

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd /c/qemu-omni-<pin>/build && ninja -j16
```

Then measure a paused guest, which is how every commit figure in this repo
was taken (`qemu_proc.py:2621-2624`) — and note the warning there: **probe
with `-accel whpx`**, because without it TCG's ~1 GB translation buffer reads
as +1070 MB.

```bash
# baseline: private commit, stock behaviour, env unset
qemu-system-x86_64.exe -accel whpx -m 3072 -display none -S -monitor none &
# read Commit Charge for that pid, then kill it

# with the file backing
QEMU_RAM_FILE_DIR="C:/Users/berat/Desktop/Omni Apps/omnidroid/scratch" \
qemu-system-x86_64.exe -accel whpx -m 3072 -display none -S -monitor none &
```

Expected, from the probe: baseline ~+3117 MB of system commit,
file-backed ~+45 MB. Record both in the commit message. **If the file-backed
figure is not within ~100 MB of the baseline's overhead-only component, stop
and report** — the probe says it should be, and a disagreement means the
RAMBlock path differs from the probe's in some way worth understanding.

- [ ] **Step 5: Export the patch and update SERIES**

```bash
cd /c/qemu-omni-<pin>
git diff -- system/physmem.c > /tmp/all-physmem.patch
# 0005 is already in that file; split so 0007 carries ONLY the new hunks
```

Split by hand: `0005` is the `#elif defined(_WIN32)` arm inside
`ram_block_discard_range`; `0007` is the include block, the new function, and
the `ram_block_add` call site. Then:

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid/qemu-patches"
printf '0007-omni-win32-ram-file.patch\n' >> SERIES
```

- [ ] **Step 6: Run the tests**

```bash
python -m pytest tests/test_qemu_patch_series.py tests/test_build_qemu.py -q
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add qemu-patches/0007-omni-win32-ram-file.patch qemu-patches/SERIES \
        tests/test_qemu_patch_series.py
git commit -m "qemu: guest RAM from a mapped sparse file on Windows

MEASURED on the target host, -m 3072, -accel whpx, paused:

  private commit (stock)   +NNNN MB of system commit
  file-backed              +NN MB

Windows charges commit for private pages and QEMU commits the whole of -m,
which is where the ~100 GB pagefile at 30 instances comes from. A mapped view
of a file is backed by that file and is not charged.

Not memory-backend-file: hostmem-file.c is excluded on Windows at
backends/meson.build:13 and file_ram_alloc is behind CONFIG_POSIX, so that
route is QAPI + meson + the whole fd allocation path. Backing the ANONYMOUS
allocation is one function and one call site and covers every RAM block.

Off unless QEMU_RAM_FILE_DIR is set; a stock invocation stays stock. Opting
in and then falling back to private commit on error would look like the
feature working while reproducing the exact pagefile it removes, so the error
path fails the boot instead."
```

---

### Task 4: Patch 0008 — a discard that punches the hole

**Files:**
- Create: `qemu-patches/0008-omni-win32-punch-hole.patch`
- Note: `include/system/ramblock.h` is patch 0007's file, not this one.
- Modify: `qemu-patches/SERIES`
- Test: `tests/test_qemu_patch_series.py` (extend `SeriesContent.EXPECTED`)

**Interfaces:**
- Consumes: `RAMBlock.omni_ram_file` (a Win32 `HANDLE`, NULL when unused) set by patch 0007.
- Produces: `ram_block_discard_range()` returning 0 on Windows for file-backed blocks, having released both the RAM and the disk. Task 7 turns on the guest-side reporting that calls it.

- [ ] **Step 1: Extend the test**

```python
# tests/test_qemu_patch_series.py -- add to SeriesContent
        "0008-omni-win32-punch-hole.patch": {"system/physmem.c"},

    def test_punch_hole_precedes_the_private_discard(self):
        """A file-backed block must take FSCTL_SET_ZERO_DATA, not
        DiscardVirtualMemory. DiscardVirtualMemory operates on private
        committed pages; against a mapped view it either fails or drops the
        pages without touching the file, which reclaims the RAM and leaks the
        disk -- and disk is the binding wall once commit is solved."""
        text = (PATCHES / "0008-omni-win32-punch-hole.patch").read_text(
            encoding="utf-8")
        self.assertIn("FSCTL_SET_ZERO_DATA", text)
        self.assertIn("rb->omni_ram_file", text)
        # An int fd field would be 0 on a g_malloc0'd block -- a valid
        # descriptor -- so every block would take this branch.
        self.assertNotIn("_get_osfhandle", text)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: FAIL — patch not on disk.

- [ ] **Step 3: Write the C**

In `ram_block_discard_range`, the `_WIN32` arm added by patch 0005 gains a
file-backed branch ahead of `DiscardVirtualMemory`:

```c
#elif defined(_WIN32)
            /* omni: two discards, because there are two backings.
             *
             * A block from omni_win32_file_ram_alloc (patch 0007) has a real
             * file behind it, and FSCTL_SET_ZERO_DATA is that file's
             * fallocate(PUNCH_HOLE): the frames are freed, the file's
             * AllocationSize drops, and the next touch reads zeroes. MEASURED
             * (tools/probes/whpx_probe.c) to work on a range that is LIVE in
             * a WHPX partition -- the hypervisor does not pin it.
             *
             * DiscardVirtualMemory is the private-commit equivalent and is
             * kept for blocks with no file. It frees the frames but leaves
             * the reservation, which is right for private memory and wrong
             * for a mapping.
             */
            if (rb->omni_ram_file) {
                HANDLE fh = (HANDLE)rb->omni_ram_file;
                FILE_ZERO_DATA_INFORMATION zd;
                DWORD junk = 0;

                zd.FileOffset.QuadPart = (LONGLONG)(rb->fd_offset + start);
                zd.BeyondFinalZero.QuadPart = zd.FileOffset.QuadPart
                                              + (LONGLONG)length;
                if (fh == INVALID_HANDLE_VALUE ||
                    !DeviceIoControl(fh, FSCTL_SET_ZERO_DATA, &zd, sizeof(zd),
                                     NULL, 0, &junk, NULL)) {
                    ret = -EIO;
                    error_report("%s: punch-hole failed %s:%" PRIx64 " +%zx",
                                 __func__, rb->idstr, offset, length);
                    goto err;
                }
                ret = 0;
            } else {
                /* ... existing DiscardVirtualMemory block from patch 0005 ... */
            }
```

- [ ] **Step 4: Build and prove the disk comes back**

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd /c/qemu-omni-<pin>/build && ninja -j16
```

Verify against a live guest rather than a paused one — a paused guest never
frees a page, so it can neither confirm nor deny this:

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_QEMU_DIR=/c/qemu-omni-staged \
  python -m omnidroid start HezMi_ImYu --mode farming --json
# then, while it runs, watch the RAM file:
powershell -NoProfile -Command "Get-ChildItem scratch/omni-ram-*.bin | \
  Select-Object Name,Length,@{n='AllocMB';e={ \
    (fsutil file queryallocatedranges \$_.FullName | Measure-Object).Count}}"
```

Expected: `Length` equals `-m`, and the allocated size stays well below it and
**falls** after the guest frees memory. Record the 30-minute figure.

- [ ] **Step 5: Export, update SERIES, run tests**

```bash
cd /c/qemu-omni-<pin> && git diff -- system/physmem.c   # split 0008 out
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid/qemu-patches"
printf '0008-omni-win32-punch-hole.patch\n' >> SERIES
cd .. && python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add qemu-patches/0008-omni-win32-punch-hole.patch qemu-patches/SERIES \
        tests/test_qemu_patch_series.py
git commit -m "qemu: a discard that gives the disk back too

A block from patch 0007 has a real file behind it, so its discard is
FSCTL_SET_ZERO_DATA -- the file's fallocate(PUNCH_HOLE). Frames freed,
AllocationSize down, next touch reads zeroes. Blocks with no file keep
DiscardVirtualMemory, which is right for private commit and wrong for a
mapping: it would reclaim the RAM and leak the disk, and disk is the binding
wall once commit is solved.

Verified on a live guest, not a paused one -- a paused guest never frees a
page and can neither confirm nor deny this."
```

---

### Task 5: Patch 0004 rewritten — the three-option close prompt

**Files:**
- Modify: `qemu-patches/0004-omni-confirm-close.patch`
- Test: `tests/test_qemu_patch_series.py` (new assertions)

**Interfaces:**
- Consumes: nothing.
- Produces: a QEMU whose window close offers Shut down / Hide the viewer / Cancel, where Hide calls `gtk_widget_hide` and leaves the VM running. Task 8 removes `window-close=off` so the X reaches it.

- [ ] **Step 1: Write the failing assertions**

```python
# tests/test_qemu_patch_series.py -- add to SeriesContent
    def test_close_prompt_offers_three_outcomes(self):
        """The user asked for exactly these three, in this order. Two of them
        are destructive-adjacent and the default must be neither."""
        text = (PATCHES / "0004-omni-confirm-close.patch").read_text(
            encoding="utf-8")
        self.assertIn("Shut down the machine", text)
        self.assertIn("Hide the viewer", text)
        self.assertIn("Cancel", text)
        # Hide must not power anything off.
        self.assertIn("gtk_widget_hide", text)
        # Cancel is the default response, so Enter and Escape are both safe.
        self.assertIn("GTK_RESPONSE_CANCEL", text)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_qemu_patch_series.py::SeriesContent -q
```
Expected: FAIL — the current patch says "Stop this instance?" and offers two
buttons.

- [ ] **Step 3: Rewrite the dialog**

Replace the `QEMU_WINDOW_CONFIRM_CLOSE` block in `gd_window_close`
(`ui/gtk.c:919`):

```c
    if (allow_close && g_getenv("QEMU_WINDOW_CONFIRM_CLOSE")) {
        /* omni: closing this window powers off a machine that took a minute
         * to boot, and the window is a VIEW onto an instance rather than the
         * instance itself -- so the X has to offer putting the view away as
         * well as stopping the machine. An accidental click is otherwise
         * unrecoverable.
         *
         * This lives in QEMU rather than in a helper process because one
         * process cannot intercept another's WM_CLOSE without injecting a
         * DLL. Every previous attempt at this was a way of losing more
         * slowly. */
        enum { OMNI_SHUTDOWN = 1, OMNI_HIDE = 2 };
        GtkWidget *omni_dlg;
        gint omni_resp;

        omni_dlg = gtk_message_dialog_new(GTK_WINDOW(s->window),
                                          GTK_DIALOG_MODAL |
                                          GTK_DIALOG_DESTROY_WITH_PARENT,
                                          GTK_MESSAGE_QUESTION,
                                          GTK_BUTTONS_NONE,
                                          "This machine is running.");
        gtk_message_dialog_format_secondary_text(
            GTK_MESSAGE_DIALOG(omni_dlg),
            "Shutting it down loses anything running inside it. "
            "Hiding the viewer keeps it running -- show it again from the "
            "app.");
        gtk_dialog_add_buttons(GTK_DIALOG(omni_dlg),
                               "Cancel", GTK_RESPONSE_CANCEL,
                               "Hide the viewer", OMNI_HIDE,
                               "Shut down the machine", OMNI_SHUTDOWN, NULL);
        gtk_dialog_set_default_response(GTK_DIALOG(omni_dlg),
                                        GTK_RESPONSE_CANCEL);
        omni_resp = gtk_dialog_run(GTK_DIALOG(omni_dlg));
        gtk_widget_destroy(omni_dlg);

        if (omni_resp == OMNI_HIDE) {
            /* The VM keeps running and the GL context is untouched -- this is
             * a widget visibility change, nothing more. The launcher learns
             * about it the same way it learns about its own hide path. */
            gtk_widget_hide(s->window);
            return TRUE;
        }
        if (omni_resp != OMNI_SHUTDOWN) {
            return TRUE;                /* Cancel, Escape, or the dialog's X */
        }
        allow_close = true;
    }
```

- [ ] **Step 4: Build and try all three by hand**

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd /c/qemu-omni-<pin>/build && ninja -j16
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_QEMU_DIR=/c/qemu-omni-staged python -m omnidroid start HezMi_ImYu --mode gaming
OMNI_QEMU_DIR=/c/qemu-omni-staged python -m omnidroid view HezMi_ImYu
```

Click the X three times and record each outcome:

| choice | expected |
|---|---|
| Cancel | window stays, `omnidroid list` still shows it running |
| Hide the viewer | window disappears, `omnidroid list` still shows it running, `omnidroid view` brings it back |
| Shut down the machine | window closes, `omnidroid list` shows it gone |

Also press Escape — it must behave as Cancel.

- [ ] **Step 5: Export the patch, run tests**

```bash
cd /c/qemu-omni-<pin> && git diff -- ui/gtk.c    # re-split 0001-0004
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
python -m pytest tests/test_qemu_patch_series.py -q
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add qemu-patches/0004-omni-confirm-close.patch \
        tests/test_qemu_patch_series.py
git commit -m "qemu: the X asks, and one of its answers is 'not really'

Three outcomes -- shut down, hide the viewer, cancel -- with cancel as the
default so Enter and Escape are both safe. Hide is gtk_widget_hide: the VM
keeps running and the GL context is untouched, because the window is a view
onto an instance rather than the instance itself.

In QEMU rather than in a helper because one process cannot intercept
another's WM_CLOSE without injecting a DLL. Verified by clicking all three."
```

---

### Task 6: Point the density profile at file-backed RAM

**Files:**
- Modify: `omnidroid/qemu_proc.py` (`_apply_window_env` region ~2311-2340; new `ram_file_env`)
- Test: `tests/test_qemu_footprint.py` (extend; it already covers per-instance command cost)

**Interfaces:**
- Consumes: patch 0007's `QEMU_RAM_FILE_DIR` contract.
- Produces: `qemu_proc.ram_file_env(env, cfg, mode) -> dict` — sets `QEMU_RAM_FILE_DIR` to `scratch_dir(cfg)` when the mode's profile is `density`, the host is Windows, and the resolved QEMU advertises the capability; returns `env` unchanged otherwise. Called from `scratch_env` (`qemu_proc.py:2295`). Task 7 consumes `qemu_supports_ram_file(cfg)`.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_qemu_footprint.py -- append
class RamFileBacking(unittest.TestCase):
    """Guest RAM is backed by a file we own, not by the system pagefile.

    The whole point is the commit charge: `-m` is charged 1:1 against the
    system commit limit when it is private, and not at all when it is a
    mapped file. See docs/superpowers/runbooks/2026-08-19-windows-ram-backing.md
    """

    def _mode(self, profile="density"):
        return {"profile": profile, "mem": 3072}

    def test_density_on_windows_gets_the_scratch_dir(self):
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=True, supported=True)
        self.assertEqual(Path(env["QEMU_RAM_FILE_DIR"]),
                         qemu_proc.scratch_dir({}))

    def test_performance_profile_is_left_alone(self):
        """Gaming follows once density has proven it, not before. A gaming
        instance is one instance; it is not what the commit limit is about,
        and a soft fault in the middle of a frame is."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode("performance"),
                                     is_windows=True, supported=True)
        self.assertNotIn("QEMU_RAM_FILE_DIR", env)

    def test_not_on_linux_or_macos(self):
        """Those hosts have madvise and a real balloon. This buys them
        nothing and would add a file to reap."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=False, supported=True)
        self.assertNotIn("QEMU_RAM_FILE_DIR", env)

    def test_a_qemu_without_the_patch_is_not_asked(self):
        """A stock binary ignores the variable, so setting it is harmless --
        but then nothing has changed and the planner must not believe the
        commit is gone. The capability gate is what the planner reads."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=True, supported=False)
        self.assertNotIn("QEMU_RAM_FILE_DIR", env)

    def test_no_scratch_dir_means_no_backing(self):
        env = qemu_proc.ram_file_env({}, cfg={"qemu": {"scratch_dir": ""}},
                                     mode=self._mode(), is_windows=True,
                                     supported=True)
        self.assertNotIn("QEMU_RAM_FILE_DIR", env)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_qemu_footprint.py -k RamFileBacking -q
```
Expected: FAIL — `AttributeError: module 'omnidroid.qemu_proc' has no attribute 'ram_file_env'`.

- [ ] **Step 3: Implement**

In `omnidroid/qemu_proc.py`, replace the stale block above `_apply_window_env`
and add the new function:

```python
# The QEMU_WINDOW_* names, and QEMU_RAM_FILE_DIR, are read by OUR QEMU --
# the one tools/build-qemu.py produces from qemu-patches/. A stock binary
# ignores every one of them, so an old build cannot break on them; it simply
# does not get the behaviour.
#
#   QEMU_WINDOW_ICON          the window's icon              (patch 0001)
#   QEMU_WINDOW_LOCK_ASPECT   WM_SIZING filter, client area  (patch 0002)
#   QEMU_WINDOW_PANEL         ui-info the guest is handed    (patch 0003)
#   QEMU_WINDOW_CONFIRM_CLOSE the three-option close prompt  (patch 0004)
#   QEMU_RAM_FILE_DIR         guest RAM from a mapped file   (patch 0007)
WINDOW_ICON_NAME = "omni-icon.png"
RAM_FILE_ENV = "QEMU_RAM_FILE_DIR"


def ram_file_env(env, cfg=None, mode=None, is_windows=None, supported=None):
    """Point guest RAM at a file in the scratch, for density on Windows.

    FOUR GATES, and every one of them is a measurement rather than a taste.

    WINDOWS ONLY. Linux and macOS have `madvise` and a balloon that decommits
    for real; this buys them nothing and costs them a file to reap.

    DENSITY ONLY, for now. The commit limit is a FLEET problem -- one gaming
    instance never approached it. What file-backing trades is commit for soft
    faults, and a soft fault in the middle of a frame is exactly what gaming
    is not allowed to have. Gaming moves over when density has held it for a
    while, not before.

    A PATCHED QEMU ONLY. A stock binary ignores the variable, which is safe
    but silent -- and `capacity_shortfall` would then plan a fleet against a
    commit cost that is still being paid. The gate is what the planner reads.

    A SCRATCH THAT EXISTS. `scratch_dir` is already the ephemeral-overlay home
    and is already reaped (`reap_scratch`), size-checked (`scratch_free_mb`)
    and redirectable (`qemu.scratch_dir`), so the RAM files inherit all of it.
    """
    if is_windows is None:
        is_windows = IS_WINDOWS
    if not is_windows:
        return env
    if ((mode or {}).get("profile")) != "density":
        return env
    if supported is None:
        supported = qemu_supports_ram_file(cfg)
    if not supported:
        return env
    d = scratch_dir(cfg)
    if d is None:
        return env
    env[RAM_FILE_ENV] = str(d)
    return env


def qemu_supports_ram_file(cfg=None):
    """Does the resolved QEMU carry patch 0007?

    Asked by running it, not by reading a version: the product has shipped
    three different QEMU builds and `--version` distinguishes none of them,
    because the patches do not bump it.
    """
    return "omni-ram-file" in _qemu_omni_caps(cfg)
```

Add the capability probe beside `_qemu_help_texts` (`qemu_proc.py:458`), and
patch 0001-0008's build to advertise it. The cheapest honest probe is a
version-string suffix, so extend `tools/build_qemu.py`'s configure call with
`--with-pkgversion=omni-ram-file+omni-window` and parse it:

```python
@functools.lru_cache(maxsize=4)
def _qemu_omni_caps(cfg=None):
    """The omni capability tokens baked into this QEMU's --version string."""
    try:
        out = subprocess.run([qemu_bin(qemu_system_name()), "--version"],
                             capture_output=True, text=True, timeout=10).stdout
    except Exception:
        return ()
    m = re.search(r"\(([^)]*omni[^)]*)\)", out)
    return tuple(m.group(1).split("+")) if m else ()
```

Then wire it in at `scratch_env` (`qemu_proc.py:2295-2308`), after
`_apply_window_env(base)`:

```python
    _apply_window_env(base)
    ram_file_env(base, cfg, mode)
    return base
```

`scratch_env` gains a `mode=None` parameter; its one caller is
`qemu_proc.py:2441` (`qemu_env = scratch_env(cfg)`), which becomes
`scratch_env(cfg, mode=mode)`.

- [ ] **Step 4: Run the tests**

```bash
python -m pytest tests/test_qemu_footprint.py -q
```
Expected: PASS.

- [ ] **Step 5: Measure a real farming instance**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_QEMU_DIR=/c/qemu-omni-staged \
  python -m omnidroid start HezMi_ImYu --mode farming --json
OMNI_QEMU_DIR=/c/qemu-omni-staged python -m omnidroid measure --json
```

Record: marginal system commit per instance (was 4065 MB), host RSS (should
stay ~384 MB — the working-set governor is unchanged), and the RAM file's
allocated size. **Do not update `COMMIT_OVERHEAD_MB` from a paused probe** —
that constant was 192 and wrong by 4x for exactly that reason
(`qemu_proc.py:2617`).

- [ ] **Step 6: Commit**

```bash
git add omnidroid/qemu_proc.py tests/test_qemu_footprint.py \
        tools/build_qemu.py
git commit -m "farming: guest RAM comes from a file we own, not the pagefile

Density on Windows sets QEMU_RAM_FILE_DIR to the scratch, so patch 0006 backs
every RAM block with a mapped sparse file. Marginal commit per instance
NNNN -> NNN MB, measured on a live farming instance, not a paused probe --
COMMIT_OVERHEAD_MB was 192 and wrong by 4x for exactly that reason.

Four gates and each is a measurement: Windows only (Linux/macOS have madvise
and a balloon that works), density only for now (what this trades is commit
for soft faults, and a soft fault mid-frame is what gaming may not have), a
patched QEMU only (a stock binary ignores the variable, which is safe but
silent -- and the planner would then size a fleet against a cost still being
paid), and a scratch that exists.

Also deletes the 'NO QEMU READS THESE' block above _apply_window_env. They
are read now."
```

---

### Task 7: `free-page-reporting=on` returns to Windows

**Files:**
- Modify: `omnidroid/qemu_proc.py:1457-1487` (`balloon_device`)
- Test: `tests/test_qemu_footprint.py` (extend)

**Interfaces:**
- Consumes: `qemu_supports_ram_file(cfg)` from Task 6.
- Produces: `balloon_device(cfg=None)` emitting `free-page-reporting=on` on Windows when the resolved QEMU can discard.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_qemu_footprint.py -- append
class FreePageReporting(unittest.TestCase):
    """It was dropped on Windows because the discard underneath it did not
    exist: 925 failed ram_block_discard_range calls a minute, 78 KB of
    qemu.log, nothing reclaimed. With patches 0005+0008 the discard succeeds,
    and the reporting is what keeps the RAM file's ALLOCATED size near the
    guest's live set instead of everything it has ever touched."""

    def test_on_when_the_discard_works(self):
        args = qemu_proc.balloon_device(is_windows=True, can_discard=True)
        self.assertIn("free-page-reporting=on", args[1])

    def test_off_on_a_qemu_that_cannot_discard(self):
        args = qemu_proc.balloon_device(is_windows=True, can_discard=False)
        self.assertNotIn("free-page-reporting", args[1])

    def test_unchanged_elsewhere(self):
        args = qemu_proc.balloon_device(is_windows=False, can_discard=False)
        self.assertIn("free-page-reporting=on", args[1])
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_qemu_footprint.py -k FreePageReporting -q
```
Expected: FAIL — `balloon_device()` takes no `is_windows`/`can_discard`.

- [ ] **Step 3: Implement**

```python
def balloon_device(cfg=None, is_windows=None, can_discard=None):
    """The virtio-balloon device, with free-page reporting where it works.

    HISTORY, because this flag has been on and off once already. Reporting was
    dropped on Windows (2026-08) when every ram_block_discard_range() returned
    -ENOSYS: 925 failures a minute, 78 KB of qemu.log, one failed discard per
    4 MB block for the life of the instance, and nothing reclaimed. The flag
    was never the problem; the discard under it was.

    Patches 0005 and 0008 give that discard a Windows implementation --
    DiscardVirtualMemory for private blocks, FSCTL_SET_ZERO_DATA for
    file-backed ones -- so the reporting now does what it says. On a
    file-backed guest it is what keeps the RAM file's ALLOCATED size tracking
    the live set rather than the high-water mark, which is the difference
    between a fleet that fits on this disk and one that does not.
    """
    if is_windows is None:
        is_windows = IS_WINDOWS
    if can_discard is None:
        can_discard = qemu_supports_ram_file(cfg)
    if is_windows and not can_discard:
        return ["-device", "virtio-balloon-pci,id=omniball"]
    return ["-device",
            "virtio-balloon-pci,free-page-reporting=on,id=omniball"]
```

Both call sites (`qemu_proc.py:1703` arm, `:1888` x86) pass `cfg`.

- [ ] **Step 4: Run the tests**

```bash
python -m pytest tests/test_qemu_footprint.py -q
```
Expected: PASS.

- [ ] **Step 5: Confirm the log is quiet and the file shrinks**

The old failure mode is loud and easy to check for:

```bash
OMNI_QEMU_DIR=/c/qemu-omni-staged \
  python -m omnidroid start HezMi_ImYu --mode farming
sleep 120
grep -c "Failed to discard\|MADVISE not available\|punch-hole failed" \
     runtime/HezMi_ImYu/qemu.log      # expect 0
ls -l runtime/HezMi_ImYu/qemu.log     # expect KB, not the old 78 KB/min
```

Then watch the RAM file's allocated size over 30 minutes of farming and record
the peak and the steady state.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/qemu_proc.py tests/test_qemu_footprint.py
git commit -m "farming: free-page reporting works on Windows now

It was dropped there when every discard under it returned -ENOSYS -- 925
failures a minute, 78 KB of qemu.log, nothing reclaimed. The flag was never
the problem. Patches 0005 and 0008 give the discard a Windows implementation,
so reporting now punches holes in the RAM file as the guest frees pages,
which is what keeps its ALLOCATED size near the live set instead of the
high-water mark.

Gated on the same capability probe as the RAM file, so an older binary keeps
the quiet behaviour rather than the log spam. Verified: 0 discard failures in
120 s where the old build produced hundreds."
```

---

### Task 8: Delete the external aspect lock, and let the X through

**Files:**
- Modify: `omnidroid/hostwin.py` (delete `_resolve_lock` :1626, `_initial_fit` :1643, `_aspect_watch` :1674, `run_aspect_lock` :1775, `aspect_lock` :1798, `ASPECT_POLL_SECONDS` :1613, `ASPECT_IDLE_POLL_SECONDS` :1620, `ASPECT_IDLE_SECONDS` :1623, `IDENTITY_RECHECK_SECONDS` :1671)
- Modify: `omnidroid/engine.py` (delete `_window_lock_pid_path` :6673, `_running_window_lock_pid` :6677, `_spawn_window_lock` :6692, `_ensure_window_lock` :6722, `maybe_start_window_lock` :6748, `_write_window_lock_pid` :6779, `_run_windowlock` :6791, the `_windowlock` subparser :12086-12095, and the call sites at :7211, :7238, :10096, :10221)
- Modify: `omnidroid/qemu_proc.py:364-369` (`_WINDOW_FLAGS` — drop `window-close=off`)
- Modify: `tests/engine_public_names.json`
- Modify: `tests/test_window_at_boot.py` (the `_ensure_window_lock` gating block, :318-365)
- Test: `tests/test_window_at_boot.py` (new assertions)

**Interfaces:**
- Consumes: patches 0002 and 0004, shipped by Task 9.
- Produces: nothing new. `aspect_fit` and `aspect_is_close` and their tests stay.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_window_at_boot.py -- replace the _ensure_window_lock block
class AspectIsHeldInsideQemu(unittest.TestCase):
    """The aspect ratio is QEMU's job now.

    It used to be a detached process calling SetWindowPos on QEMU's window
    every 8 ms during Windows' modal size loop -- correcting the drag AFTER
    the fact, from OUTSIDE the process, up to 125 times a second. It held the
    ratio (2 of 246 samples off) and it flickered, and it could not be fixed
    where it stood: hostwin.py said so itself -- Windows will not let one
    process handle another's WM_SIZING without injecting a DLL.

    Patch 0002 handles WM_SIZING inside the drag loop, in QEMU, which is the
    only place it can be handled. These tests assert the outside machinery is
    gone rather than merely unused."""

    def test_no_external_lock_remains(self):
        for name in ("aspect_lock", "run_aspect_lock", "_aspect_watch",
                     "_initial_fit", "_resolve_lock",
                     "ASPECT_POLL_SECONDS", "ASPECT_IDLE_POLL_SECONDS"):
            self.assertFalse(hasattr(hostwin, name),
                             f"hostwin.{name} survived the deletion")

    def test_no_windowlock_subcommand(self):
        parser = engine.build_parser()
        actions = [a for a in parser._subparsers._group_actions]
        self.assertNotIn("_windowlock", actions[0].choices)

    def test_the_arithmetic_stays_as_the_specification(self):
        """aspect_fit is now the readable statement of what the C does.
        1280x800 is 1.6; a 1600-wide drag must give 1000 high."""
        self.assertEqual(hostwin.aspect_fit(1600, 940, 1280, 800,
                                            axis="width"), (1600, 1000))

    def test_qemu_is_told_to_lock_the_aspect(self):
        env = qemu_proc._apply_window_env({})
        self.assertEqual(env["QEMU_WINDOW_LOCK_ASPECT"], "1")


class TheCloseBoxIsLive(unittest.TestCase):
    """window-close=off existed because on a stock binary the X killed the
    guest instantly, mid-boot, with no prompt. Patch 0004 prompts, so the X
    is allowed to reach it."""

    def test_gtk_no_longer_disables_the_close_box(self):
        flags = qemu_proc.window_flags("gtk")
        self.assertNotIn("window-close=off", flags)
        self.assertIn("keep-aspect-ratio=on", flags)

    def test_qemu_is_told_to_confirm(self):
        env = qemu_proc._apply_window_env({})
        self.assertEqual(env["QEMU_WINDOW_CONFIRM_CLOSE"], "1")
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/test_window_at_boot.py -k "AspectIsHeld or CloseBox" -q
```
Expected: FAIL — `hostwin.aspect_lock` still exists; `window-close=off` still
in the gtk flags.

- [ ] **Step 3: Delete**

In `omnidroid/qemu_proc.py`, `_WINDOW_FLAGS` becomes:

```python
# The X is live again. It was disabled because on a stock binary clicking it
# powered the guest off instantly -- measured, mid-boot, no prompt, nothing to
# undo it. Patch 0004 puts a three-option prompt behind it (shut down / hide
# the viewer / cancel), which is what the flag was standing in for. `sdl` and
# `cocoa` keep window-close=off because neither backend has the patch.
_WINDOW_FLAGS = {
    "gtk":   ("show-menubar=off", "zoom-to-fit=on", "keep-aspect-ratio=on"),
    "sdl":   ("window-close=off",),
    "cocoa": ("zoom-to-fit=on",),
}
```

Delete the nine `hostwin.py` symbols and the seven `engine.py` ones listed in
**Files**. At each engine call site, delete the call, not just its result:

- `engine.py:7211` — inside `cmd_view`'s show path, drop `_ensure_window_lock(args.name)`
- `engine.py:7238` — `locked = _ensure_window_lock(args.name)` and the `"locked": locked` it feeds into the JSON result; report `"aspect": "qemu"` instead so a reader can tell the two eras apart
- `engine.py:10096` and `:10221` — drop `maybe_start_window_lock(acct, label)`

Then remove the deleted public names from `tests/engine_public_names.json`
(`_run_windowlock`, `maybe_start_window_lock`, and any of the others listed
there — grep the file).

- [ ] **Step 4: Run the tests**

```bash
python -m pytest tests/test_window_at_boot.py tests/test_facade_equivalence.py \
                tests/test_hidden_window_viewer.py -q
```
Expected: PASS. If `test_hidden_window_viewer.py` fails on a removed name,
update it — it patched `_spawn_window_bar`, not the lock, so failures there
mean a call site was missed.

- [ ] **Step 5: Watch a real drag**

The claim is "smooth", and smooth is not a unit test.

```bash
OMNI_QEMU_DIR=/c/qemu-omni-staged \
  python -m omnidroid start HezMi_ImYu --mode gaming
OMNI_QEMU_DIR=/c/qemu-omni-staged python -m omnidroid view HezMi_ImYu
```

Drag a corner slowly for ~10 seconds, then a side, then maximise and restore.
Record: does the picture stay edge-to-edge with no letterbox bars, does the
opposite edge stay put, and is there any visible flicker or snapping. Confirm
no `omnidroid _windowlock` process exists (`tasklist | grep -i omnidroid`).

- [ ] **Step 6: Commit**

```bash
git add omnidroid/hostwin.py omnidroid/engine.py omnidroid/qemu_proc.py \
        tests/engine_public_names.json tests/test_window_at_boot.py
git commit -m "window: the flicker's mechanism is deleted, not tuned

The aspect ratio was held by a detached process calling SetWindowPos on
QEMU's window every 8 ms during Windows' modal size loop -- correcting the
drag after the fact, from outside the process, up to 125 times a second. It
held the ratio and it flickered, and hostwin.py already said why it could not
be fixed where it stood: Windows will not let one process handle another's
WM_SIZING without injecting a DLL.

Patch 0002 handles WM_SIZING inside the drag loop. So _windowlock,
_aspect_watch, aspect_lock, run_aspect_lock, _initial_fit, _resolve_lock and
the whole engine-side pid plumbing go. aspect_fit/aspect_is_close stay with
their tests as the readable statement of what the C now does.

window-close=off comes off gtk too. It existed because on a stock binary the
X powered the guest off instantly mid-boot with no prompt; patch 0004 is the
prompt. sdl and cocoa keep it -- neither backend has the patch."
```

---

### Task 9: Ship it

**Files:**
- Modify: `omnidroid/engine.py` (`cmd_qemu_info` :5847, `cmd_doctor` :5973)
- Modify: `tools/build_qemu.py` (`--with-pkgversion`)
- Create: `docs/superpowers/runbooks/qemu-build.md`
- Test: `tests/test_qemu_accepts_devices.py` (extend — it already runs a REAL QEMU)

**Interfaces:**
- Consumes: everything above.
- Produces: `engine.qemu_build_report(cfg) -> dict` with keys `path`, `version`, `omni_caps` (tuple), `targets` (list) — surfaced by both `qemu-info --json` and `doctor --json`.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_qemu_accepts_devices.py -- append
class OmniBuildIsIdentifiable(unittest.TestCase):
    """Three different QEMU builds have shipped in this product and
    `--version` distinguished none of them, because a patch does not bump it.
    `doctor` has to be able to say which one a machine has, from the shipped
    binary, without launching a guest."""

    def test_report_names_the_binary_and_its_capabilities(self):
        rep = engine.qemu_build_report({})
        self.assertTrue(Path(rep["path"]).exists())
        self.assertRegex(rep["version"], r"^\d+\.\d+\.\d+")
        self.assertIsInstance(rep["omni_caps"], tuple)

    def test_a_patched_build_advertises_both_capabilities(self):
        rep = engine.qemu_build_report({})
        if not rep["omni_caps"]:
            self.skipTest("stock QEMU on this host; nothing to assert")
        self.assertIn("omni-window", rep["omni_caps"])
        self.assertIn("omni-ram-file", rep["omni_caps"])

    def test_the_real_binary_accepts_the_window_flags_we_emit(self):
        """QEMU refuses an unknown display suboption OUTRIGHT rather than
        ignoring it, so a wrong flag costs the boot, not the chrome."""
        flags = qemu_proc.window_flags("gtk")
        if not flags:
            self.skipTest("not a window-flag platform")
        out = subprocess.run(
            [qemu_proc.qemu_bin(qemu_proc.qemu_system_name()),
             "-display", f"gtk,{flags}", "-machine", "none", "-S",
             "-monitor", "none"],
            capture_output=True, text=True, timeout=20)
        self.assertNotIn("Invalid parameter", out.stderr)
        self.assertNotIn("not supported", out.stderr)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  python -m pytest tests/test_qemu_accepts_devices.py -k OmniBuild -q
```
Expected: FAIL — `engine.qemu_build_report` does not exist.

- [ ] **Step 3: Implement**

Add to `tools/build_qemu.py`'s `_COMMON_FLAGS`:

```python
    "--with-pkgversion=omni-window+omni-ram-file",
```

And to `omnidroid/engine.py`, beside `cmd_qemu_info`:

```python
def qemu_build_report(cfg=None):
    """Which QEMU is this, and what can it do.

    `--version` alone has never been enough: three builds have shipped and a
    patch does not bump the version, so 11.1.0 has meant 'stock', 'window
    patches only', and 'window + RAM file' on different machines in the same
    week. The pkgversion suffix is the answer, because it is baked at
    configure time by tools/build-qemu.py and travels with the binary.
    """
    path = qemu_bin(qemu_system_name())
    try:
        out = subprocess.run([path, "--version"], capture_output=True,
                             text=True, timeout=10).stdout
    except Exception as exc:
        return {"path": path, "version": None, "omni_caps": (),
                "targets": [], "error": str(exc)}
    ver = re.search(r"version (\d+\.\d+\.\d+)", out)
    return {
        "path": path,
        "version": ver.group(1) if ver else None,
        "omni_caps": qemu_proc._qemu_omni_caps(cfg),
        "targets": [n for n in ("qemu-system-x86_64", "qemu-system-aarch64")
                    if Path(qemu_bin(n)).exists()],
    }
```

Then include `qemu_build_report(cfg)` in both `cmd_qemu_info`'s and
`cmd_doctor`'s JSON payload under the key `"qemu_build"`.

- [ ] **Step 4: Run the tests**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q
```
Expected: the baseline 11 failures and no new ones. Diff the `FAILED` lines
against the recorded baseline; do not count them.

- [ ] **Step 5: Install the build and verify from the shipped path**

```bash
# stage into the runtime dir the product actually uses -- NOT <exe dir>/qemu,
# which the app's updater renames aside on every update
cp -r /c/qemu-omni-staged/* "$LOCALAPPDATA/OmniExec/qemu/"
python -m omnidroid doctor --json | python -m json.tool | grep -A6 qemu_build
python -m omnidroid start HezMi_ImYu --mode gaming --json
python -m omnidroid start HezMi_ImYu --mode farming --json
```

Expected: `omni_caps` lists both tokens, both targets present, and both modes
reach a live client. Remember the standing rule — **verify the frozen build,
not the source**; if a release follows, re-run `--doctor` from the shipped
`omni-exec.exe`, not from this checkout.

- [ ] **Step 6: Write the runbook**

Create `docs/superpowers/runbooks/qemu-build.md` covering: prerequisites
(MSYS2 packages, already installed here), the exact `tools/build-qemu.py`
invocation for Windows and for the Mac, how to add a patch to the series, how
to verify a build carries the capabilities, and where the staged bundle goes
on each platform (`%LOCALAPPDATA%\OmniExec\qemu` on Windows; a private prefix
with `qemu.dir` pointed at it on macOS — **never** over the Homebrew `qemu`
formula, which `startergo/qemu-virgl-kosmickrisp` will evict).

- [ ] **Step 7: Commit**

```bash
git add omnidroid/engine.py tools/build_qemu.py \
        tests/test_qemu_accepts_devices.py \
        docs/superpowers/runbooks/qemu-build.md
git commit -m "qemu: doctor can say which build a machine has

Three QEMU builds have shipped and --version distinguished none of them,
because a patch does not bump it -- 11.1.0 has meant stock, window-patches,
and window+RAM-file on different machines in the same week. The pkgversion
suffix is baked at configure time and travels with the binary, so
qemu_build_report reads it from the resolved path without launching a guest.

Also asserts, against the REAL binary, that the display suboptions we emit
are accepted. QEMU refuses an unknown one outright rather than ignoring it,
so a wrong flag costs the boot, not the chrome."
```

---

## Self-Review

**Spec coverage.** §3a → Task 1. §3b → Task 5. §3c → Tasks 3, 6. §3d → Task 7.
§3e → Tasks 2, 9. §3f → Task 8. §8's measurement table: commit → Task 6 Step 5;
punch-hole/live-set → Task 4 Step 4 and Task 7 Step 5; smooth resize → Task 8
Step 5; three close outcomes → Task 5 Step 4; GPU check and fleet number are
sub-project A/B, out of scope here and named as such.

**Gap accepted deliberately:** §3c describes the feature as
`memory-backend-file`; Task 3 implements it as an env-gated anonymous-alloc
replacement instead, and says why in the task and in its commit message. The
observable behaviour and every measured number are the same; the surface is
smaller. Update the spec's §3c wording when Task 3 lands.

**Type consistency.** `ram_file_env(env, cfg, mode, is_windows, supported)`,
`qemu_supports_ram_file(cfg)`, `_qemu_omni_caps(cfg)`,
`balloon_device(cfg, is_windows, can_discard)`, `qemu_build_report(cfg)`,
`read_series(patches_dir)`, `read_pin(patches_dir)`,
`configure_argv(prefix, targets, extra, host_os, source)`,
`apply_argv(patch, check)`, `stage_plan(build_dir, out_dir, targets)` — each
defined once and used with the same signature everywhere it appears.
