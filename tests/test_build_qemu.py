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
