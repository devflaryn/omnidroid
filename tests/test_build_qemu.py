"""The QEMU build is a script, not a shell history.

Everything here is pure: argv construction and plan shape. Nothing compiles.
The build itself is verified by running it (Task 2 Step 5), because a build
is not a thing a unit test can pin.
"""
import tempfile
import unittest
from pathlib import Path

from tools.build_qemu import (_enclosing_function, _imports_of,
                              aligned_discard_interior, apply_argv,
                              collect_runtime_dlls, configure_argv, read_pin,
                              read_series, stage_plan, verify_applied)

REPO = Path(__file__).resolve().parent.parent
PATCHES = REPO / "qemu-patches"


class AlignedDiscardInterior(unittest.TestCase):
    """Pure arithmetic, no I/O -- the reference for patch 0008's C punch-hole
    rounding. See aligned_discard_interior()'s docstring for the measurement
    (tools/probes/sparse_granularity.c) that makes this arithmetic load-
    bearing rather than an optimization: an unaligned or sub-unit punch
    reclaims 0 bytes on NTFS, confirmed at 4 KiB/32 KiB/unaligned-64 KiB.
    """

    GRANULARITY = 64 * 1024  # omni_win32_alloc_granularity() on the
                              # measured host; the function takes it as a
                              # parameter precisely so it is not baked in.

    def test_sub_unit_range_is_empty(self):
        # A single 4 KiB balloon page: never a whole aligned unit.
        offset, length = aligned_discard_interior(0, 4096, self.GRANULARITY)
        self.assertEqual(length, 0)

    def test_already_aligned_whole_unit_is_itself(self):
        offset, length = aligned_discard_interior(
            self.GRANULARITY, self.GRANULARITY, self.GRANULARITY)
        self.assertEqual((offset, length), (self.GRANULARITY, self.GRANULARITY))

    def test_unaligned_128kib_range_yields_the_64kib_interior(self):
        # Starts 4 KiB into the first unit, so only the second of the two
        # units it touches is ever fully covered.
        offset, length = aligned_discard_interior(
            4096, 128 * 1024, self.GRANULARITY)
        self.assertEqual((offset, length),
                         (self.GRANULARITY, self.GRANULARITY))

    def test_range_starting_and_ending_mid_unit_is_empty(self):
        # Half of unit 0 plus half of unit 1 -- spans a unit boundary, a
        # full 64 KiB of bytes, but covers no single whole unit.
        half = self.GRANULARITY // 2
        offset, length = aligned_discard_interior(
            half, self.GRANULARITY, self.GRANULARITY)
        self.assertEqual(length, 0)


class Series(unittest.TestCase):
    def test_reads_in_order_and_skips_comments(self):
        names = [p.name for p in read_series(PATCHES)]
        self.assertEqual(names[0], "0001-omni-window-identity.patch")
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

    def test_pkgversion_advertises_host_cursor_from_0010(self):
        """`+omni-host-cursor` is the token qemu_proc reads to know the
        omni-host-cursor QMP command is there; it must come from 0010's
        presence and nothing else."""
        def pv(argv):
            return next(a.split("=", 1)[1] for a in argv
                        if a.startswith("--with-pkgversion="))
        base = [Path("0001-omni-window-identity.patch")]
        self.assertNotIn("omni-host-cursor",
                         pv(configure_argv(Path("/o"), ["x86_64-softmmu"],
                                           series=base)))
        with10 = base + [Path("0010-omni-host-cursor.patch")]
        self.assertTrue(pv(configure_argv(Path("/o"), ["x86_64-softmmu"],
                                          series=with10))
                        .endswith("+omni-host-cursor"))

    def test_pkgversion_is_derived_from_the_series(self):
        """A capability string that can drift from the patches is worse than
        none: Task 6 reads it to decide whether to back guest RAM with a
        file, and Task 7 to decide whether to turn free-page reporting
        on."""
        def pv(argv):
            return next(a.split("=", 1)[1] for a in argv
                        if a.startswith("--with-pkgversion="))
        base = [Path("0001-omni-window-identity.patch")]
        self.assertEqual(pv(configure_argv(Path("/o"), ["x86_64-softmmu"],
                                           series=base)), "omni-window")
        with7 = base + [Path("0007-omni-win32-ram-file.patch")]
        self.assertEqual(pv(configure_argv(Path("/o"), ["x86_64-softmmu"],
                                           series=with7)),
                         "omni-window+omni-ram-file")
        with8 = with7 + [Path("0008-omni-win32-punch-hole.patch")]
        self.assertEqual(pv(configure_argv(Path("/o"), ["x86_64-softmmu"],
                                           series=with8)),
                         "omni-window+omni-ram-file+omni-punch-hole")

    def test_punch_hole_token_cannot_appear_without_its_patch(self):
        """0005's DiscardVirtualMemory arm fails on a mapped view, so
        free-page reporting must not be enableable before 0008 lands."""
        with7 = [Path("0007-omni-win32-ram-file.patch")]
        argv = configure_argv(Path("/o"), ["x86_64-softmmu"], series=with7)
        self.assertNotIn("omni-punch-hole", " ".join(argv))


class Apply(unittest.TestCase):
    def test_checks_before_it_applies(self):
        # A patch that will not apply must not half-apply. `git apply` is
        # atomic per invocation, so the contract is simply: one invocation.
        argv = apply_argv(Path("/p/0001.patch"))
        self.assertEqual(argv[:2], ["git", "apply"])
        self.assertIn("--check", apply_argv(Path("/p/0001.patch"), check=True))

    def test_reverse_check_argv(self):
        # main()'s idempotent apply loop uses `--reverse --check` to tell
        # "already applied" (exit 0) from "not applied yet" (nonzero)
        # without mutating the tree either way.
        argv = apply_argv(Path("/p/0001.patch"), check=True, reverse=True)
        self.assertIn("--check", argv)
        self.assertIn("--reverse", argv)
        self.assertEqual(argv[:2], ["git", "apply"])


class EnclosingFunctionIsNotFooled(unittest.TestCase):
    """The guard exists for future rebases, and a rebase is exactly when a
    comment containing example code with a brace shows up. Reporting the
    WRONG function is worse than reporting nothing."""

    def test_brace_in_a_comment_does_not_shift_scope(self):
        # The comment's brace is UNBALANCED (opens a scope it never closes).
        # A balanced pair like `if (x) { y(); }` nets back to the right depth
        # by literal counting alone, so it would pass even without the
        # comment-blanking guard this test exists to prove -- this fixture
        # does not net back, so only real blanking gets "beta" right.
        src = (
            'static void alpha(void)\n{\n'
            '    /* opens a scope: if (x) { */\n'
            '    int a;\n}\n\n'
            'static void beta(void)\n{\n'
            '    const char *panel = g_getenv("QEMU_WINDOW_PANEL");\n}\n'
        )
        self.assertEqual(_enclosing_function(src, "QEMU_WINDOW_PANEL"), "beta")

    def test_brace_in_a_string_literal_does_not_shift_scope(self):
        src = (
            'static void alpha(void)\n{\n'
            '    printf("{{{");\n}\n\n'
            'static void beta(void)\n{\n'
            '    const char *panel = g_getenv("QEMU_WINDOW_PANEL");\n}\n'
        )
        self.assertEqual(_enclosing_function(src, "QEMU_WINDOW_PANEL"), "beta")

    def test_brace_in_a_char_literal_does_not_shift_scope(self):
        src = (
            "static void alpha(void)\n{\n"
            "    char c = '{';\n}\n\n"
            "static void beta(void)\n{\n"
            '    const char *panel = g_getenv("QEMU_WINDOW_PANEL");\n}\n'
        )
        self.assertEqual(_enclosing_function(src, "QEMU_WINDOW_PANEL"), "beta")

    def test_attribute_macro_after_the_signature(self):
        src = ('static void beta(void) QEMU_ATTR(unused)\n{\n'
               '    const char *panel = g_getenv("QEMU_WINDOW_PANEL");\n}\n')
        self.assertEqual(_enclosing_function(src, "QEMU_WINDOW_PANEL"), "beta")

    def test_a_prototype_does_not_become_the_scope(self):
        """Restored after fix round 1: mutation-tested against the CURRENT
        implementation, not just the historical pre-fix one. Deleting the
        depth-0 ';' clear (the branch that ends a prototype's `pending`)
        makes this fixture return 'alpha' instead of 'beta' -- because
        without it, `pending` survives the semicolon, and the "only set
        pending while it is None" guard then refuses to let beta's own
        name-paren overwrite the stale value. (An EARLIER version of this
        test used the same fixture but reasoned about a different, no-
        longer-current baseline -- an implementation with no such guard at
        all, which always overwrites `pending` regardless of the ';' clear,
        so the fixture could not have caught that baseline's absence of a
        guard. It still catches loss of the ';' clear in the code as it
        exists today, which is a real and separately mutable piece of
        behaviour with no other coverage.)"""
        src = ('static void alpha(void);\n\n'
               'static void beta(void)\n{\n'
               '    const char *panel = g_getenv("QEMU_WINDOW_PANEL");\n}\n')
        self.assertEqual(_enclosing_function(src, "QEMU_WINDOW_PANEL"), "beta")


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

    # omni: a decoy occurrence sitting EARLIER in the file and INSIDE
    # want_fn (gd_set_ui_size), while the real hunk landed LATER, in the
    # wrong function (gd_set_ui_refresh_rate). `text.find()` -- the first
    # occurrence -- resolves to the decoy, reports clean, and never looks at
    # the real one. This is the false negative the hardened check exists to
    # close: some real anchor symbol legitimately repeating itself (e.g. as
    # a second, unrelated `g_getenv()` call) must not let a misplaced hunk
    # hide behind it.
    DECOY_THEN_WRONG = """
static void gd_set_ui_size(VirtualConsole *vc, gint width, gint height)
{
    /* decoy: an unrelated read of the same env var, earlier in the file
     * and inside the function the anchor is supposed to guard. */
    const char *decoy = g_getenv("QEMU_WINDOW_PANEL");
}

static void gd_set_ui_refresh_rate(VirtualConsole *vc, int refresh_rate)
{
    /* the REAL hunk, landed in the wrong function */
    const char *panel = g_getenv("QEMU_WINDOW_PANEL");
}
"""

    # omni: the exact shape of two of the three real false positives this
    # hardening round fixed -- an occurrence sitting at FILE SCOPE (a
    # comment, or a bodyless declaration), plus the real occurrence inside
    # want_fn. File-scope occurrences must be ignored, not fatal.
    FILE_SCOPE_DECOY_THEN_REAL = """
/* example: g_getenv("QEMU_WINDOW_PANEL") controls panel pinning */

static void gd_set_ui_size(VirtualConsole *vc, gint width, gint height)
{
    const char *panel = g_getenv("QEMU_WINDOW_PANEL");
}
"""

    # omni: the symbol appears inside two different functions, NEITHER of
    # which is want_fn -- must be reported as an ambiguous anchor, not
    # silently resolved to whichever the first occurrence happens to sit in.
    TWO_DIFFERENT_FUNCTIONS = """
static void gd_update_caption(VirtualConsole *vc)
{
    const char *panel = g_getenv("QEMU_WINDOW_PANEL");
}

static void gd_window_close(VirtualConsole *vc)
{
    const char *panel = g_getenv("QEMU_WINDOW_PANEL");
}
"""

    def test_decoy_in_want_fn_does_not_hide_a_real_hunk_elsewhere(self):
        """Must now be caught: a decoy inside want_fn used to let `find()`'s
        first-match resolve clean while the real hunk sat in the wrong
        function, unreported."""
        out = verify_applied(self._tree(self.DECOY_THEN_WRONG),
                             [Path("0003-omni-panel-pin.patch")])
        self.assertTrue(any("QEMU_WINDOW_PANEL" in v for v in out),
                        f"a real hunk in the wrong function, hidden behind "
                        f"a decoy in want_fn, went unreported: {out}")

    def test_file_scope_decoy_does_not_block_a_clean_apply(self):
        out = verify_applied(self._tree(self.FILE_SCOPE_DECOY_THEN_REAL),
                             [Path("0003-omni-panel-pin.patch")])
        self.assertEqual([v for v in out if "QEMU_WINDOW_PANEL" in v], [])

    def test_occurrences_in_two_functions_are_reported_as_ambiguous(self):
        out = verify_applied(self._tree(self.TWO_DIFFERENT_FUNCTIONS),
                             [Path("0003-omni-panel-pin.patch")])
        matches = [v for v in out if "QEMU_WINDOW_PANEL" in v]
        self.assertTrue(matches, "two-function occurrence went unreported")
        self.assertTrue(any("ambig" in v.lower() for v in matches),
                        f"message should name the real problem (ambiguous "
                        f"anchor), not point at a wrong function: {matches}")

    def test_anchors_for_absent_patches_are_not_asserted(self):
        """0007 and 0008 do not exist until tasks 3 and 4. Their anchors must
        not fail a build that legitimately has not got them yet."""
        out = verify_applied(self._tree(self.GOOD), [])
        self.assertEqual(out, [])


class ResumingAFullyPatchedTree(unittest.TestCase):
    """⚠ A tree carrying the WHOLE series must be resumable.

    The per-patch probe asks `git apply --reverse --check <patch>` -- "would
    undoing this patch succeed" -- which is the right question only if the
    patches are independent. They are not: 0008 edits the very hunk 0005
    added. So on a fully patched tree, reverse-checking 0005 fails (its text
    is no longer what 0005 wrote) AND the forward check fails (it IS applied),
    and the loop refused with "neither applies cleanly nor is already
    applied" on a tree that was perfectly correct. Re-running after a failed
    ninja -- the normal way anyone uses this script -- was impossible.

    Undo order is the reverse of apply order, so the all-or-nothing probe
    walks the series BACKWARDS. Asserted here on order alone, with no git and
    no tree, because that is the whole of the fix.
    """

    def test_the_series_is_probed_backwards(self):
        import inspect
        from tools import build_qemu
        src = inspect.getsource(build_qemu.main)
        self.assertIn("reversed(series)", src,
                      "the already-applied probe must walk the series in "
                      "UNDO order, or an interdependent patch pair makes a "
                      "correct tree unresumable")

    def test_a_partial_tree_still_falls_through_to_the_refusal(self):
        """All-or-nothing on purpose. A HALF-applied tree is the state nobody
        should be guessing about, and it must still reach the per-patch loop
        and its SystemExit rather than being waved through."""
        import inspect
        from tools import build_qemu
        src = inspect.getsource(build_qemu.main)
        self.assertIn("neither applies cleanly nor is already", src)
        self.assertIn("all(", src)


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

    def test_firmware_comes_from_the_SOURCE_tree_as_well(self):
        """⚠ THE BUNDLE CANNOT BOOT WITHOUT THIS.

        QEMU ships the x86 firmware PREBUILT in the source tree -- 28 blobs,
        bios-256k.bin / vgabios-*.bin / kvmvapic.bin -- and never copies them
        into the build directory; `build/pc-bios` holds only what the build
        generates (edk2 .fd images, descriptors, dtb). Staged from the build
        dir alone the bundle has every UEFI blob and NO BIOS, and QEMU exits
        with "could not load PC BIOS" before it logs anything -- which on
        this engine's boot path reads exactly like a GPU that killed the
        guest, so the GPU gets blamed and disabled. Cost a build to find on
        2026-09-02.
        """
        plan = stage_plan(Path("/src/build"), Path("/out"), ["x86_64-softmmu"],
                          source_dir=Path("/src"))
        shares = [src for src, dst in plan if dst.name == "share"]
        self.assertEqual(len(shares), 2, "both pc-bios trees must be staged")
        self.assertIn(Path("/src/pc-bios"), shares)
        self.assertIn(Path("/src/build/pc-bios"), shares)
        # Source first: the build's generated blobs must WIN where they
        # overlap, and the copy is dirs_exist_ok so later entries overlay.
        self.assertLess(shares.index(Path("/src/pc-bios")),
                        shares.index(Path("/src/build/pc-bios")))

    def test_source_dir_defaults_to_the_build_parent(self):
        """Every path in this script runs configure from <source>/build, so
        the default keeps older callers correct rather than silently staging
        nothing."""
        plan = stage_plan(Path("/src/build"), Path("/out"), ["x86_64-softmmu"])
        shares = [src for src, dst in plan if dst.name == "share"]
        self.assertIn(Path("/src/pc-bios"), shares)


class ImportsOf(unittest.TestCase):
    """Pinned against REAL `objdump -p qemu-img.exe` output (captured on
    the build host against the actual staged binary, 2026-08-19), not a
    hand-written guess at the format -- so the regex is proven against
    what objdump actually emits, not what someone remembers it emitting.
    Trimmed to three import-table blocks (a handful of entries each); the
    surrounding vma/hint/thunk table rows are kept verbatim specifically
    to prove they are NOT mistaken for "DLL Name:" lines.
    """

    CAPTURED = (
        "There is an import table in .idata at 0x401c3000\n"
        "\n"
        "The Import Tables (interpreted .idata section contents)\n"
        " vma:            Hint    Time      Forward  DLL       First\n"
        "                 Table   Stamp     Chain    Name      Thunk\n"
        " 001c3000\t001c30f0 00000000 00000000 001c6e44 001c3fa8\n"
        "\n"
        "\tDLL Name: ADVAPI32.dll\n"
        "\tvma:     Ordinal  Hint  Member-Name  Bound-To\n"
        "\t001c3fa8  <none>  00c1  CryptAcquireContextA\n"
        "\t001c3fb0  <none>  00d2  CryptGenRandom\n"
        "\n"
        " 001c3014\t001c3108 00000000 00000000 001c6e60 001c3fc0\n"
        "\n"
        "\tDLL Name: libbz2-1.dll\n"
        "\tvma:     Ordinal  Hint  Member-Name  Bound-To\n"
        "\t001c3fc0  <none>  0007  BZ2_bzDecompress\n"
        "\t001c3fc8  <none>  0008  BZ2_bzDecompressEnd\n"
        "\n"
        " 001c3028\t001c3128 00000000 00000000 001c6fdc 001c3fe0\n"
        "\n"
        "\tDLL Name: KERNEL32.dll\n"
        "\tvma:     Ordinal  Hint  Member-Name  Bound-To\n"
        "\t001c3fe0  <none>  0000  AcquireSRWLockExclusive\n"
        "\t001c3fe8  <none>  0025  AreFileApisANSI\n"
    )

    def test_extracts_every_dll_name_in_order(self):
        self.assertEqual(_imports_of(self.CAPTURED),
                         ["ADVAPI32.dll", "libbz2-1.dll", "KERNEL32.dll"])

    def test_member_names_are_not_mistaken_for_dll_names(self):
        # CryptAcquireContextA, BZ2_bzDecompress etc. sit in the SAME
        # column layout one line below "DLL Name:" -- a looser regex could
        # grab them too. Exactly three matches, none of them a symbol name.
        names = _imports_of(self.CAPTURED)
        self.assertEqual(len(names), 3)
        self.assertNotIn("CryptAcquireContextA", names)

    def test_no_import_table_yields_no_names(self):
        self.assertEqual(_imports_of("nothing relevant here\n"), [])


class CollectRuntimeDlls(unittest.TestCase):
    """`_imports_fn` replaces the real `objdump -p` call, so these fixtures
    are small fakes -- a dict of basename -> its declared imports -- rather
    than compiled PE files. The search dir still needs real (empty) files:
    that half of the classification (found on disk or not) is exactly the
    behaviour under test, and it costs nothing to fake with zero-byte
    placeholders instead of a second layer of mocking.
    """

    def _search_dir(self, tmp, names):
        d = Path(tmp) / "search"
        d.mkdir(exist_ok=True)
        for name in names:
            (d / name).write_bytes(b"")
        return d

    def test_a_system_dll_is_not_bundled(self):
        with tempfile.TemporaryDirectory() as tmp:
            search = self._search_dir(tmp, [])  # nothing third-party exists
            imports = {"root.exe": ["KERNEL32.dll", "ADVAPI32.dll"]}
            dlls = collect_runtime_dlls(
                [Path(tmp) / "root.exe"], [search], Path(tmp) / "out",
                _imports_fn=lambda p: imports.get(Path(p).name, []))
            self.assertEqual(dlls, [])

    def test_a_dll_present_in_the_search_dir_is_bundled(self):
        with tempfile.TemporaryDirectory() as tmp:
            search = self._search_dir(tmp, ["foo.dll"])
            out = Path(tmp) / "out"
            imports = {"root.exe": ["foo.dll", "KERNEL32.dll"], "foo.dll": []}
            dlls = collect_runtime_dlls(
                [Path(tmp) / "root.exe"], [search], out,
                _imports_fn=lambda p: imports.get(Path(p).name, []))
            self.assertEqual(dlls, ["foo.dll"])
            self.assertTrue((out / "foo.dll").is_file(),
                            "found-in-search-dir dependency was not copied")

    def test_walk_is_recursive(self):
        # root -> a.dll -> b.dll -> c.dll, each only named by its parent's
        # import table -- if the walk stopped after one hop, b.dll and
        # c.dll would never be discovered at all.
        with tempfile.TemporaryDirectory() as tmp:
            search = self._search_dir(tmp, ["a.dll", "b.dll", "c.dll"])
            imports = {
                "root.exe": ["a.dll"],
                "a.dll": ["b.dll"],
                "b.dll": ["c.dll"],
                "c.dll": [],
            }
            dlls = collect_runtime_dlls(
                [Path(tmp) / "root.exe"], [search], Path(tmp) / "out",
                _imports_fn=lambda p: imports.get(Path(p).name, []))
            self.assertEqual(dlls, ["a.dll", "b.dll", "c.dll"])

    def test_deterministic_across_repeat_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            search = self._search_dir(
                tmp, ["a.dll", "b.dll", "c.dll", "d.dll"])
            imports = {
                "root.exe": ["d.dll", "a.dll", "KERNEL32.dll"],
                "a.dll": ["b.dll", "c.dll"],
                "b.dll": [],
                "c.dll": [],
                "d.dll": [],
            }
            def fake(p):
                return imports.get(Path(p).name, [])

            first = collect_runtime_dlls(
                [Path(tmp) / "root.exe"], [search], Path(tmp) / "out",
                copy=False, _imports_fn=fake)
            second = collect_runtime_dlls(
                [Path(tmp) / "root.exe"], [search], Path(tmp) / "out",
                copy=False, _imports_fn=fake)
            self.assertEqual(first, second)
            self.assertEqual(first, ["a.dll", "b.dll", "c.dll", "d.dll"])


if __name__ == "__main__":
    unittest.main()
