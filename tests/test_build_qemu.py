"""The QEMU build is a script, not a shell history.

Everything here is pure: argv construction and plan shape. Nothing compiles.
The build itself is verified by running it (Task 2 Step 5), because a build
is not a thing a unit test can pin.
"""
import tempfile
import unittest
from pathlib import Path

from tools.build_qemu import (_enclosing_function, aligned_discard_interior,
                              apply_argv, configure_argv, read_pin,
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
