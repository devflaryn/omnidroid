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
#
# --with-pkgversion is set HERE and nowhere else: later tasks read it back
# out of `--version` to detect capabilities, and a second task appending it
# is how a duplicate lands.
#
# It says only "omni-window" because that is all THIS series has: patch
# 0007 (the RAM file backing) is Task 3's work and does not exist yet. Task
# 3 appends "+omni-ram-file" here once 0007 lands -- not before, because
# qemu_supports_ram_file() (Task 6) trusts this string verbatim to decide
# whether to set QEMU_RAM_FILE_DIR, and a tag that claims a capability the
# binary does not have is worse than no tag: QEMU would silently ignore the
# env var while the caller believed file-backed RAM was in effect.
_COMMON_FLAGS = (
    "--enable-gtk",
    "--enable-opengl",
    "--enable-virglrenderer",
    "--enable-slirp",
    "--disable-docs",
    "--disable-werror",
    "--with-pkgversion=omni-window",
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
#
# The symbol must be something `text.find()` locates at the ONE spot that
# matters. That ruled out two entries verified against the real v11.1.0
# tree (see Step 4b): a function's own NAME is a bad anchor when the patch
# both defines and calls it, because the definition (file scope) always
# precedes the call and `find()` returns the definition every time --
# reporting a false violation on a perfectly clean apply. Anchor on a
# symbol that appears exactly once at the site being guarded instead (an
# env var name, a Win32 API name, a macro).
_ANCHORS = (
    ("ui/gtk.c", "QEMU_WINDOW_TITLE", "gd_update_caption", "0001"),
    # Not "omni_install_aspect_filter": that name's first occurrence is its
    # own definition (file scope, added just above gd_update_geometry_hints
    # by this same patch), not the call inside gd_update_geometry_hints.
    ("ui/gtk.c", "QEMU_WINDOW_LOCK_ASPECT", "gd_update_geometry_hints",
     "0002"),
    ("ui/gtk.c", "QEMU_WINDOW_PANEL", "gd_set_ui_size", "0003"),
    ("ui/gtk.c", "QEMU_WINDOW_CONFIRM_CLOSE", "gd_window_close", "0004"),
    # The Win32 discard block patch 0005 adds lands in
    # ram_block_discard_shared_range, not ram_block_discard_range -- see the
    # patch's own hunk header (`@@ ... ram_block_discard_shared_range`).
    ("system/physmem.c", "DiscardVirtualMemory",
     "ram_block_discard_shared_range", "0005"),
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

    `pending` deliberately survives newlines: QEMU's own style puts a
    function's opening brace on its own line (`foo(...)\n{`), so clearing
    `pending` on '\n' would forget the signature before the brace that
    consumes it -- and every top-level definition in the file would report
    as file-scope. `pending` is safe to carry across lines because it is
    only ever read at the next depth-0 '{', and any '(' seen before that at
    depth 0 (whether on this line or a later one) overwrites it first.
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
        if ch == "(" and depth == 0:
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
    cfg_argv = configure_argv(a.out, targets, source=work)
    if sys.platform.startswith("win"):
        # configure is a POSIX shell script (`#!/bin/sh`); Windows'
        # CreateProcess has no shebang support, so invoked bare it fails
        # with "not a valid Win32 application". configure_argv() itself
        # still returns the bare argv (argv[0] ending in "configure") --
        # that contract is what Task 9 and the tests rely on -- this
        # wrapping is purely how main() executes it on this host.
        cfg_argv = [shutil.which("sh") or "sh", *cfg_argv]
    _run(cfg_argv, cwd=build)
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
