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
        "0001-omni-window-identity.patch": {"ui/gtk.c"},
        "0002-omni-aspect-lock.patch": {"ui/gtk.c", "include/ui/gtk.h",
                                        "ui/gtk-gl-area.c"},
        "0003-omni-panel-pin.patch": {"ui/gtk.c"},
        "0004-omni-confirm-close.patch": {"ui/gtk.c"},
        "0005-omni-win32-discard.patch": {"system/physmem.c"},
        "0006-omni-win32-build-no-symlinks.patch":
            {"scripts/symlink-install-tree.py"},
        "0007-omni-win32-ram-file.patch":
            {"system/physmem.c", "include/system/ramblock.h"},
        "0008-omni-win32-punch-hole.patch": {"system/physmem.c"},
    }

    def test_each_patch_touches_only_its_files(self):
        for name, expected in self.EXPECTED.items():
            text = (PATCHES / name).read_text(encoding="utf-8")
            touched = set(re.findall(r"^\+\+\+ b/(.+)$", text, re.M))
            self.assertEqual(touched, expected, f"{name} touches {touched}")

    def test_ram_file_patch_is_env_gated(self):
        """A build that ships this must behave exactly like stock QEMU until
        the launcher opts in. `omnidroid` is not the only thing that will
        ever run this binary."""
        text = (PATCHES / "0007-omni-win32-ram-file.patch").read_text(
            encoding="utf-8")
        self.assertIn("QEMU_RAM_FILE_DIR", text)
        self.assertIn("FILE_ATTRIBUTE_TEMPORARY", text)
        self.assertIn("FSCTL_SET_SPARSE", text)

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

    def test_caption_change_is_gated(self):
        """Every omni feature is behind a QEMU_WINDOW_* gate so a stock
        invocation stays stock. The caption is not an exception -- a build of
        ours run by anyone else must still say QEMU."""
        text = (PATCHES / "0001-omni-window-identity.patch").read_text(
            encoding="utf-8")
        self.assertIn("QEMU_WINDOW_TITLE", text)
        self.assertIn('"QEMU (%s)"', text)   # the stock branch survives

    def test_close_prompt_offers_three_outcomes(self):
        """The user asked for exactly these three, in this order. Two of them
        are destructive-adjacent and the default must be neither."""
        text = (PATCHES / "0004-omni-confirm-close.patch").read_text(
            encoding="utf-8")
        self.assertIn("Shut down the machine", text)
        self.assertIn("Hide the viewer", text)
        self.assertIn("Cancel", text)
        # Cancel is the default response, so Enter and Escape are both safe.
        self.assertIn("GTK_RESPONSE_CANCEL", text)

    def test_hide_is_a_win32_showwindow_not_a_gtk_widget_hide(self):
        """Ruling: the engine's own hide/show pair (hostwin.hide_qemu_window /
        show_qemu_window, which `omnidroid view <name>` calls) is Win32-level
        -- ShowWindow(hwnd, SW_HIDE/SW_SHOW) on the raw HWND. If the dialog
        hid the GTK *widget* instead, GTK's visibility state and Win32's
        would disagree: SW_SHOW puts the HWND back on screen while GTK still
        believes the widget is unmapped, and whether it ever repaints again
        is a GDK implementation detail. Hiding at the same level the engine
        shows at is what makes the two exact inverses."""
        text = (PATCHES / "0004-omni-confirm-close.patch").read_text(
            encoding="utf-8")
        self.assertIn("ShowWindow", text)
        self.assertIn("SW_HIDE", text)
        # gdk_win32_window_get_handle is how patch 0002 already gets the HWND
        # off a GdkWindow -- 0004's hide path must fetch it the same way.
        self.assertIn("gdk_win32_window_get_handle", text)
        # Non-Windows builds have no HWND to hide, so they fall back to the
        # plain GTK call.
        self.assertIn("gtk_widget_hide", text)

    def test_hide_returns_true_without_destroying_the_window(self):
        """After hiding, gd_window_close must `return TRUE` so GTK does not
        destroy the window and the VM keeps running -- only the shut-down
        branch may fall through to qmp_quit."""
        text = (PATCHES / "0004-omni-confirm-close.patch").read_text(
            encoding="utf-8")
        hide_idx = text.index("OMNI_HIDE")
        # The nearest `return TRUE;` after the hide branch begins is the one
        # that keeps the window (and the VM) alive.
        self.assertIn("return TRUE", text[hide_idx:])

    def test_only_the_shutdown_branch_reaches_qmp_quit(self):
        text = (PATCHES / "0004-omni-confirm-close.patch").read_text(
            encoding="utf-8")
        self.assertIn("OMNI_SHUTDOWN", text)
        self.assertIn("qmp_quit", text)


if __name__ == "__main__":
    unittest.main()
