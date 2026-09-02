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
import re
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
# --with-pkgversion used to live here as a hand-written literal. That was a
# design error: a second task appending to it by hand is how a duplicate
# lands, but a FIRST task writing the wrong literal (0007 landed, string
# still said plain "omni-window"; or the reverse) is exactly as real a
# failure mode and nothing caught it either time it happened during this
# series. _pkgversion() computes the string from the series actually being
# applied instead, so it cannot drift from what the binary has. See
# _pkgversion()'s docstring for the ordering hazard this closes.
_COMMON_FLAGS = (
    "--enable-gtk",
    "--enable-opengl",
    "--enable-virglrenderer",
    "--enable-slirp",
    "--disable-docs",
    "--disable-werror",
)


def _pkgversion(series) -> str:
    """The --with-pkgversion string, derived from the series being applied
    rather than written by hand -- later tasks read it back out of
    `--version` to detect capabilities, and a literal that can drift from
    what actually got patched in is worse than no literal at all.

    qemu_supports_ram_file() (Task 6) trusts the `+omni-ram-file` token to
    decide whether to set QEMU_RAM_FILE_DIR. A future Task 7 is expected to
    trust `+omni-punch-hole` the same way for free-page reporting. The two
    are separate tokens, not one, because of a real ordering hazard: with
    QEMU_RAM_FILE_DIR set and patch 0007 applied but 0008 NOT applied,
    0005's DiscardVirtualMemory arm runs against a mapped FILE VIEW instead
    of private committed pages. DiscardVirtualMemory only works on private
    commit, so every discard then fails and takes the `-EIO` error path --
    reinstating the exact host-keeps-paying-for-freed-guest-memory failure
    this whole sub-project exists to remove, and doing it as a hard error
    where stock behaviour was merely `-ENOSYS`. Deriving `+omni-ram-file`
    from 0007's presence and `+omni-punch-hole` from 0008's, off the SAME
    series, makes "file backing on, punch-hole off" structurally impossible
    to advertise rather than a thing a reader has to remember not to do.
    """
    numbers = {p.name[:4] for p in series}
    tag = "omni-window"
    if "0007" in numbers:
        tag += "+omni-ram-file"
    if "0008" in numbers:
        tag += "+omni-punch-hole"
    if "0009" in numbers:
        tag += "+omni-refresh"
    return tag


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
                   host_os: str | None = None, source: Path | None = None,
                   series=()):
    """argv for QEMU's ./configure.

    The accelerator flag is host-selected because `--enable-whpx` on a
    non-Windows host does not warn, it FAILS configure -- which on the Mac
    would read as "the patch series is broken".

    `series` is the list of patch Paths actually being applied (typically
    `read_series()`'s return value), NOT read from disk here -- this
    function stays pure and testable, callable with a synthetic series a
    unit test invents, rather than reaching into qemu-patches/SERIES on its
    own. See `_pkgversion()` for what it does with it and why the resulting
    --with-pkgversion flag matters beyond cosmetics.
    """
    cfg = (source or Path(".")) / "configure"
    argv = [str(cfg), f"--prefix={prefix.as_posix()}",
            "--target-list=" + ",".join(targets)]
    argv.extend(_COMMON_FLAGS)
    argv.append(f"--with-pkgversion={_pkgversion(series)}")
    argv.append(_ACCEL_FLAG[_host_key(host_os)])
    argv.extend(extra)
    return argv


def apply_argv(patch: Path, check: bool = False,
               reverse: bool = False) -> list[str]:
    """argv for applying (or probing) one patch. One invocation, so it is
    atomic.

    `check=True, reverse=True` together are how main() tests whether a
    patch is ALREADY applied (`git apply --reverse --check` exits 0 iff
    reversing it would succeed, i.e. the tree already has it) -- see the
    idempotent apply loop in main() and Finding 2 of the fix round that
    added this parameter.
    """
    argv = ["git", "apply"]
    if check:
        argv.append("--check")
    if reverse:
        argv.append("--reverse")
    argv.append(str(patch))
    return argv


def aligned_discard_interior(offset: int, length: int, granularity: int):
    """(aligned_offset, aligned_length) of the largest `granularity`-aligned
    sub-range fully contained in [offset, offset + length). `aligned_length`
    is 0 -- an empty interior -- when the range is smaller than one whole
    unit, or straddles a unit boundary without ever fully covering one.

    This is the reference for patch 0008's C arithmetic (the punch-hole
    branch of `ram_block_discard_shared_range`'s Win32 arm), not a copy that
    can drift silently: MEASURED with `tools/probes/sparse_granularity.c`
    against a live NTFS volume, `FSCTL_SET_ZERO_DATA` reclaims disk ONLY for
    a punch that is a whole, aligned unit --

        punch 4 KiB  @ 1 MiB (aligned)       ok=1  reclaimed=0.000 MB
        punch 32 KiB @ 2 MiB (aligned)       ok=1  reclaimed=0.000 MB
        punch 64 KiB @ 3 MiB (aligned)       ok=1  reclaimed=0.062 MB   <- whole unit
        punch 64 KiB @ 4 MiB+4K (UNaligned)  ok=1  reclaimed=0.000 MB
        punch 1 MiB  @ 8 MiB (aligned)       ok=1  reclaimed=1.000 MB

    -- and returns success either way, so a caller cannot tell "reclaimed"
    from "zero-filled and still allocated" from the return value alone. A
    per-4-KiB virtio-balloon page discard is exactly the worst case: every
    one of 2166 real calls against a live guest (Task 4's live-guest
    measurement) returned success and reclaimed nothing, because none of
    them were ever a whole aligned unit. Rounding the punch INWARD to what
    can actually be reclaimed, and skipping the ioctl -- not even
    attempting it -- when nothing whole remains, is what turns "thousands
    of syscalls a minute under free-page-reporting that buy nothing" into
    "only the calls that can work." `granularity` is the caller's own
    `omni_win32_alloc_granularity()` (64 KiB on the host this was measured
    on), not a hardcoded 65536 -- it stays right if a volume's allocation
    unit ever differs.
    """
    start = offset
    end = offset + length
    aligned_start = ((start + granularity - 1) // granularity) * granularity
    aligned_end = (end // granularity) * granularity
    if aligned_start >= aligned_end:
        return (aligned_start, 0)
    return (aligned_start, aligned_end - aligned_start)


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
    # Revised in Task 4's hardening round. NOT "QEMU_WINDOW_LOCK_ASPECT":
    # that env var is checked in THREE different ui/gtk.c functions by
    # design (gd_update_geometry_hints, gd_update_windowsize, and a third
    # from an unrelated later hunk) -- a real, intentional multi-call-site
    # feature, not a misplaced hunk. Against the hardened checker (every
    # occurrence, not just the first) that is correctly reported as an
    # ambiguous anchor, which it always secretly was; the old first-match
    # checker just happened to resolve its first occurrence to the right
    # function and never looked at the other two.
    # "omni_install_aspect_filter" was rejected here in Task 2 for the
    # opposite reason: its first occurrence is its own definition (file
    # scope, added just above gd_update_geometry_hints by this same patch),
    # which the OLD checker treated as a violation. The hardened checker
    # ignores file-scope occurrences, so that objection no longer applies,
    # and the symbol has exactly one non-file-scope occurrence -- the call
    # inside gd_update_geometry_hints -- making it the actually-unique
    # anchor Task 2 was looking for.
    ("ui/gtk.c", "omni_install_aspect_filter", "gd_update_geometry_hints",
     "0002"),
    ("ui/gtk.c", "QEMU_WINDOW_PANEL", "gd_set_ui_size", "0003"),
    ("ui/gtk.c", "QEMU_WINDOW_CONFIRM_CLOSE", "gd_window_close", "0004"),
    # The Win32 discard block patch 0005 adds lands in
    # ram_block_discard_shared_range, not ram_block_discard_range -- see the
    # patch's own hunk header (`@@ ... ram_block_discard_shared_range`).
    ("system/physmem.c", "DiscardVirtualMemory",
     "ram_block_discard_shared_range", "0005"),
    # Not "omni_win32_file_ram_alloc": that name's first occurrence is its
    # own definition (file scope, this same patch adds it just above
    # ram_block_add), not the call inside ram_block_add -- the exact trap
    # this module's docstring warns about, and it was live here until
    # Task 3 actually ran verify_applied() against the built worktree and
    # got a false violation on a clean apply. "omni_err" is declared and
    # used only at the call site inside ram_block_add.
    ("system/physmem.c", "omni_err", "ram_block_add", "0007"),
    # Not "ram_block_discard_range": that's the thin public wrapper (calls
    # ram_block_discard_shared_range, then ram_block_discard_guest_memfd_
    # range) -- it contains no Win32 code at all. The branch this patch adds
    # sits in the SAME function 0005's DiscardVirtualMemory arm is already
    # anchored to, right beside it. Wrong on the first attempt here too:
    # verified by applying the real patch to the built worktree and reading
    # back _enclosing_functions() before trusting this row (see Task 4's
    # report).
    ("system/physmem.c", "FSCTL_SET_ZERO_DATA",
     "ram_block_discard_shared_range", "0008"),
)


def _blank_comments_and_literals(text: str) -> str:
    """`text` with the contents of `//`/`/* */` comments and `"..."`/'...'
    string/char literals replaced by spaces, length and newlines preserved.

    A brace inside an example snippet in a comment, or inside a string or
    char constant, is not a scope boundary -- but `_enclosing_function`'s
    brace counter cannot tell the difference on its own. This is the guard
    against exactly that: it exists for a FUTURE rebase, which is precisely
    the event most likely to introduce a comment with example code in it.
    Length is preserved (spaces substituted, not deleted) so that offsets
    computed against the blanked text still line up with the original.
    """
    out = list(text)
    i, n = 0, len(text)
    while i < n:
        two = text[i:i + 2]
        if two == "//":
            j = i
            while j < n and text[j] != "\n":
                out[j] = " "
                j += 1
            i = j
        elif two == "/*":
            end = text.find("*/", i + 2)
            end = end + 2 if end != -1 else n
            for j in range(i, end):
                if text[j] != "\n":
                    out[j] = " "
            i = end
        elif text[i] in ("\"", "'"):
            quote = text[i]
            out[i] = " "
            j = i + 1
            while j < n:
                if text[j] == "\\" and j + 1 < n:
                    out[j] = " "
                    if text[j + 1] != "\n":
                        out[j + 1] = " "
                    j += 2
                    continue
                if text[j] != "\n":
                    out[j] = " "
                if text[j] == quote:
                    j += 1
                    break
                j += 1
            i = j
        else:
            i += 1
    return "".join(out)


def _owner_prefix(scan: str):
    """`prefix` such that `prefix[i]` is the name of the C function whose
    body encloses offset `i` in `scan` (or None at file scope), for every
    `i` from 0 through `len(scan)` inclusive.

    One forward brace-counting pass computes the owner at EVERY offset, not
    just one -- shared by `_enclosing_function` (first occurrence only) and
    `_enclosing_functions` (every occurrence, see its docstring for why a
    single first-match resolution is not enough). `scan` must already have
    comments and string/char literals blanked (`_blank_comments_and_literals`)
    so a brace inside either cannot perturb the depth count.

    `pending` deliberately survives newlines: QEMU's own style puts a
    function's opening brace on its own line (`foo(...)\n{`), so clearing
    `pending` on '\n' would forget the signature before the brace that
    consumes it -- and every top-level definition in the file would report
    as file-scope. `pending` is only set while it is still `None`, and is
    cleared at every depth-0 '}' (end of a definition) and every depth-0
    ';' (end of a prototype or a bodyless declaration) -- so the first
    identifier-before-'(' since the last such boundary wins, which is the
    function's own name, not an attribute macro
    (`foo(void) SOME_ATTR(x)\n{`) or a second call on the same line.
    """
    depth = 0
    current = None
    pending = None
    prefix = [None] * (len(scan) + 1)
    for pos, ch in enumerate(scan):
        prefix[pos] = current
        if ch == "(" and depth == 0:
            if pending is None:
                # remember the identifier immediately before this paren
                j = pos
                while j > 0 and (scan[j - 1].isalnum() or scan[j - 1] == "_"):
                    j -= 1
                pending = scan[j:pos] or None
        elif ch == "{":
            if depth == 0:
                current = pending
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                current = None
                pending = None
        elif ch == ";" and depth == 0:
            pending = None
    prefix[len(scan)] = current
    return prefix


def _enclosing_function(text: str, needle: str):
    """Name of the C function whose body contains the first `needle`.

    Brace-counting from the top of the file rather than a regex, because the
    thing being guarded against is a symbol landing in the WRONG function --
    and a regex that searches backwards for the nearest `foo(...)` finds a
    call site as readily as a definition. Returns None if `needle` is absent
    or sits at file scope.

    Comments and string/char literals are blanked first (see
    `_blank_comments_and_literals`) so a brace inside either of them cannot
    perturb the depth count -- reporting the WRONG enclosing function is
    worse than reporting none, and a rebase is exactly the event likely to
    add a comment containing example code with a brace in it. `needle`
    ITSELF is located in the original, unblanked text, though: every real
    anchor symbol here is a string literal's contents
    (`g_getenv("QEMU_WINDOW_PANEL")`), so blanking would erase the very
    thing being searched for. Blanking only ever affects which function the
    brace-counter thinks `idx` falls inside, never whether `idx` is found.

    First-occurrence-only: use `_enclosing_functions` (plural) when a symbol
    might legitimately repeat, which is exactly the case `verify_applied`
    has to handle -- see that function's docstring.
    """
    idx = text.find(needle)
    if idx < 0:
        return None
    return _owner_prefix(_blank_comments_and_literals(text))[idx]


def _enclosing_functions(text: str, needle: str):
    """Enclosing function name (or None for a file-scope occurrence) for
    EVERY occurrence of `needle` in `text`, in file order.

    Exists because `_enclosing_function`'s first-match resolution has a real
    false-negative direction: a decoy occurrence sitting EARLIER in the file
    and INSIDE the expected function, while the real hunk landed LATER, in
    the wrong function, makes `text.find()` resolve to the decoy, report
    clean, and never look at the real one. `verify_applied` instead wants
    every occurrence's owner, so it can tell "some occurrence is in want_fn
    and nothing is anywhere else" from "an occurrence exists somewhere it
    shouldn't."
    """
    if needle not in text:
        return []
    prefix = _owner_prefix(_blank_comments_and_literals(text))
    out = []
    start = 0
    while True:
        idx = text.find(needle, start)
        if idx < 0:
            break
        out.append(prefix[idx])
        start = idx + 1
    return out


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

    REQUIRED SEMANTICS, and why a plain `text.find()` first-match is not
    enough: some occurrence of `symbol` must lie inside `want_fn`, AND no
    occurrence may lie inside a DIFFERENT function. File-scope occurrences
    (a comment, a symbol's own declaration) are ignored rather than fatal --
    every one of three real false positives this check has thrown was
    exactly that, caught by hand: a function's own file-scope definition, an
    env-var name in a file-scope comment, and an API name in a patch header
    comment. But the same first-match resolution that produced those false
    positives also has a false-negative direction, and that one is the
    dangerous one: a decoy occurrence sitting EARLIER in the file and INSIDE
    want_fn, while the real hunk landed LATER, in the wrong function, makes
    `text.find()` resolve to the decoy, report clean, and never look at the
    real one. So every occurrence is checked (`_enclosing_functions`, not
    `_enclosing_function`): occurrences spread across two or more different
    functions are reported as an ambiguous anchor -- naming the real problem
    (the symbol no longer identifies one call site) instead of pointing at
    whichever function happened to be found -- rather than silently or
    misleadingly resolved to a single "wrong" function.
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
        in_fn = sorted({f for f in _enclosing_functions(text, symbol)
                        if f is not None})
        if len(in_fn) > 1:
            out.append(f"{rel}: {symbol} appears inside multiple functions "
                       f"{in_fn} -- ambiguous anchor for patch {patch_num}; "
                       f"it no longer identifies one call site "
                       f"(expected only {want_fn!r})")
        elif not in_fn or in_fn[0] != want_fn:
            got = in_fn[0] if in_fn else None
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


_DLL_NAME_RE = re.compile(r"^\s*DLL Name:\s*(\S+)\s*$", re.MULTILINE)


def _imports_of(text: str) -> list[str]:
    """DLL names one PE file imports, from `objdump -p <file>` output, in
    the order objdump lists them (duplicates possible; the caller dedupes).

    Pure string parsing -- no subprocess, no filesystem -- specifically so
    it can be pinned against CAPTURED objdump output in a test with no
    compiler and no real .exe/.dll on disk. `objdump -p` prints one
    "\tDLL Name: <name>" line per imported library, inside "The Import
    Tables" section; that exact prefix does not otherwise occur in -p
    output (the export table has no per-entry DLL name), so a line-anchored
    regex is enough -- no need to track section headers.
    """
    return _DLL_NAME_RE.findall(text)


def _objdump_imports(path: Path, objdump: str) -> list[str]:
    """`_imports_of`, fed by actually running `objdump -p` on `path`."""
    result = subprocess.run([objdump, "-p", str(path)], stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, check=True)
    return _imports_of(result.stdout.decode("utf-8", errors="replace"))


def _index_search_dir(d: Path) -> dict[str, Path]:
    """lowercase filename -> Path, for the regular files directly inside
    `d` (non-recursive -- mingw64/bin is flat).

    Lowercased because a PE import record and the Windows filesystem
    entry for the same DLL are not guaranteed to agree on case (import
    tables often carry the exact case the *linker* used, which is not
    necessarily the case the file was installed with), and DLL lookup on
    Windows is case-insensitive regardless.
    """
    out: dict[str, Path] = {}
    if d.is_dir():
        for p in sorted(d.iterdir()):
            if p.is_file():
                out.setdefault(p.name.lower(), p)
    return out


def collect_runtime_dlls(binaries, search_dirs, out_dir, *, copy=True,
                         objdump="objdump", _imports_fn=None) -> list[str]:
    """Third-party runtime DLLs `binaries` need, walked recursively, and
    (when `copy` is true) copied into `out_dir`. Returns the sorted list of
    DLL filenames collected.

    WHY THIS EXISTS. `scripts/symlink-install-tree.py` normally assembles
    QEMU's run tree, including its DLL dependencies, by symlinking from the
    toolchain's lib dirs -- but patch 0006 disables exactly that on
    Windows (Developer Mode is not something a customer machine can be
    assumed to have), and `stage_plan()`'s plain file copies never picked
    up the slack. A staged bundle that only works on a machine with
    MSYS2 or Git-for-Windows on PATH is not portable; it happens to run on
    the machine that built it.

    THE CLASSIFICATION RULE. Each binary's PE import table lists the DLLs
    it loads by name, with no distinction between "ships with Windows" and
    "ships with the toolchain" -- that distinction only exists by asking
    whether a same-named FILE sits in one of `search_dirs` (mingw64/bin in
    practice). Found there -> third-party, gets copied and its own imports
    are walked in turn. Not found there -> assumed a genuine Windows
    system DLL (KERNEL32.dll, ADVAPI32.dll, the api-ms-win-core-*.dll API
    sets, ...) and left alone -- bundling those is not just wasted size,
    it risks shipping a copy that fights the real one Windows resolves at
    a different privilege level.

    `_imports_fn`, when given, replaces the real `objdump -p` invocation
    with `_imports_fn(path) -> list[str]` -- this is how the tests exercise
    recursion and classification with small fake dependency graphs instead
    of compiled PE files; production code never passes it and gets the
    real `_objdump_imports`.

    Traversal order does not affect the result: `collected` is built as a
    dict keyed by lowercased name (case-insensitive dedupe -- the same DLL
    reachable from two different binaries is copied once), and the return
    value is `sorted()` over the final set, not accumulated in visit order.
    Two calls against the same inputs therefore produce byte-identical
    output regardless of which binary's import table happens to mention a
    shared dependency first.
    """
    imports_of = _imports_fn or (lambda p: _objdump_imports(Path(p), objdump))

    index: dict[str, Path] = {}
    for d in search_dirs:
        for name, path in _index_search_dir(Path(d)).items():
            index.setdefault(name, path)

    collected: dict[str, Path] = {}   # lowercased DLL name -> its file
    visited: set[str] = set()         # files already walked for imports
    queue = [Path(b) for b in binaries]

    while queue:
        current = queue.pop(0)
        vkey = str(current).lower()
        if vkey in visited:
            continue
        visited.add(vkey)
        for dep in imports_of(current):
            key = dep.lower()
            if key in collected:
                continue          # already found and queued
            found = index.get(key)
            if found is None:
                continue           # a genuine Windows system DLL -- skip it
            collected[key] = found
            queue.append(found)   # recurse into the DLL's own imports

    if copy:
        out_dir = Path(out_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        for path in collected.values():
            shutil.copy2(path, out_dir / path.name)

    return sorted(path.name for path in collected.values())


def _default_dll_dir() -> Path | None:
    """Directory to search for third-party runtime DLLs, resolved from
    whichever `objdump` PATH would actually run -- not a hardcoded MSYS2
    install location, so a toolchain installed somewhere else still
    resolves correctly. None if no `objdump` is on PATH; main() then
    requires an explicit `--dll-dir`."""
    found = shutil.which("objdump")
    return Path(found).resolve().parent if found else None


def _run(argv, cwd=None, env=None):
    print("+", " ".join(str(a) for a in argv), flush=True)
    subprocess.run(argv, cwd=cwd, env=env, check=True)


def _probe(argv, cwd=None) -> bool:
    """Run argv and report success as a bool, output discarded.

    For probes (`git apply --check`, `... --reverse --check`) where a
    non-zero exit is an ordinary, expected outcome -- not something that
    should raise, the way `_run`'s `check=True` deliberately does for a
    real action.
    """
    result = subprocess.run(argv, cwd=cwd, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    return result.returncode == 0


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
    ap.add_argument("--dll-dir", type=Path, default=None,
                    help="search dir for third-party runtime DLLs "
                         "(default: the directory containing the `objdump` "
                         "found on PATH)")
    a = ap.parse_args(argv)

    targets = [t for t in a.targets.split(",") if t]
    pin = read_pin()
    work = a.source.parent / f"qemu-omni-{pin}"

    if not work.exists():
        _run(["git", "-C", str(a.source), "worktree", "add",
              str(work), pin])

    # Idempotent on purpose: a routine retry after a failed `ninja` is the
    # normal way anyone re-runs this script, and `work` from a prior run
    # already has the whole series applied -- re-applying blindly would die
    # on patch 0001 with an unhandled CalledProcessError. `--reverse
    # --check` exits 0 iff the patch is ALREADY applied (reversing it would
    # succeed), so that is checked before ever attempting a forward apply.
    series = read_series()
    for patch in series:
        if _probe(apply_argv(patch, check=True, reverse=True), cwd=work):
            print(f"+ already applied: {patch.name}", flush=True)
            continue
        if not _probe(apply_argv(patch, check=True), cwd=work):
            raise SystemExit(
                f"{patch.name} neither applies cleanly nor is already "
                "applied -- the tree is in a state this script does not "
                "recognize; refusing to guess")
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
    cfg_argv = configure_argv(a.out, targets, source=work, series=series)
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
    plan = stage_plan(build, a.out, targets)
    for src, dst in plan:
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        else:
            shutil.copy2(src, dst)
        print(f"staged {dst}", flush=True)

    # The staged binaries above are just files copied out of `build/` --
    # their DLL dependencies are not among them, because `stage_plan` only
    # ever knew about the emulator/tool binaries and pc-bios/. On Windows
    # those binaries need ~50 third-party DLLs from the mingw toolchain
    # (patch 0006 disables the symlink-install-tree script that would
    # normally have supplied them -- see collect_runtime_dlls()'s
    # docstring). Skipped on non-Windows hosts: there is nothing to
    # collect, and `objdump -p` output is a PE-specific format.
    if _host_key() == "win32":
        dll_dir = a.dll_dir or _default_dll_dir()
        if dll_dir is None:
            raise SystemExit(
                "no --dll-dir given and no `objdump` on PATH -- cannot "
                "resolve where the mingw runtime DLLs live, and the staged "
                "bundle will not start without them (STATUS_DLL_NOT_FOUND) "
                "on any machine that lacks MSYS2/Git-for-Windows")
        staged_binaries = [dst for _, dst in plan if dst.is_file()]
        dlls = collect_runtime_dlls(staged_binaries, [dll_dir], a.out)
        print(f"staged {len(dlls)} runtime DLL(s) from {dll_dir}",
              flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
