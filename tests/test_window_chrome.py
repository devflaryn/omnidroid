#!/usr/bin/env python3
"""QEMU's window is restyled IN PLACE, and it keeps its own native frame.

    python3 -m pytest tests/test_window_chrome.py -q

THE CAPTION STAYS, and that is a reversal worth reading before you "fix" it.
It used to be stripped, because a separate Tk strip (`windowbar.py`) was going
to be the title bar and two captions on screen is one it is impossible to
click. That strip is gone -- QEMU's own window IS the window now, which is
what was asked for -- so stripping the caption would leave a window with no
title, no icon and nothing to drag it by. WS_THICKFRAME is asserted for the
same reason it always was: the window has to stay resizable, because holding
it at the guest's aspect ratio (hostwin.aspect_lock) is only meaningful if the
user can resize it in the first place.

Nothing here may raise: a host where the chrome cannot be applied gets a plain
window and a printed reason, never a failed boot.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import hostwin


class FakeUser32:
    """Just enough of user32 to record what was asked of it."""

    def __init__(self, style=0xCF0000):
        self.style = style
        self.icons = []
        self.positions = []
        self.rect = (100, 100, 1380, 900)

    def GetWindowLongPtrW(self, hwnd, index):
        return self.style

    def SetWindowLongPtrW(self, hwnd, index, value):
        self.style = value
        return 1

    def SendMessageW(self, hwnd, msg, wparam, lparam):
        self.icons.append((msg, wparam, lparam))
        return 0

    def SetWindowPos(self, hwnd, after, x, y, cx, cy, flags):
        self.positions.append((x, y, cx, cy, flags))
        return 1

    def GetWindowRect(self, hwnd, out):
        out.left, out.top, out.right, out.bottom = self.rect
        return 1


class ChromeOnWindows(unittest.TestCase):

    def setUp(self):
        self.u = FakeUser32()
        self.patches = [
            mock.patch.object(hostwin, "backend",
                              return_value=hostwin.BACKEND_WIN32),
            mock.patch.object(hostwin, "find_window", return_value=4242),
            mock.patch.object(hostwin, "_user32", return_value=self.u),
        ]
        for p in self.patches:
            p.start()
        self.addCleanup(lambda: [p.stop() for p in self.patches])

    def test_the_caption_is_kept(self):
        # See the module docstring: there is no second title bar to replace it
        # with any more, so taking it away leaves a window with no title, no
        # icon, and nothing to drag it by.
        result = hostwin.apply_chrome("omni-farm3")
        self.assertTrue(result["applied"], result["reason"])
        self.assertTrue(self.u.style & hostwin.WS_CAPTION)

    def test_the_sizing_border_is_kept(self):
        hostwin.apply_chrome("omni-farm3")
        self.assertTrue(self.u.style & hostwin.WS_THICKFRAME)

    def test_geometry_is_restored_when_given(self):
        hostwin.apply_chrome("omni-farm3", geometry=(10, 20, 800, 600))
        self.assertIn((10, 20, 800, 600),
                      [p[:4] for p in self.u.positions])

    def test_a_missing_window_is_a_reason_not_an_exception(self):
        with mock.patch.object(hostwin, "find_window", return_value=None):
            result = hostwin.apply_chrome("omni-gone")
        self.assertFalse(result["applied"])
        self.assertIn("no window", result["reason"].lower())

    def test_a_failing_win32_call_is_a_reason_not_an_exception(self):
        with mock.patch.object(hostwin, "_user32",
                               side_effect=OSError("boom")):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertFalse(result["applied"])
        self.assertNotEqual(result["reason"], "")

    def test_geometry_is_read_back(self):
        # The brief's own fake writes into `out.left`/`out.top`/... on the
        # object `GetWindowRect` is handed -- which works against THIS fake,
        # but not against the real implementation, which must pass
        # `ctypes.byref(rect)` to the real GetWindowRect (a `byref` object
        # has no settable attributes, so a fake built this way can never
        # stand in for it). `window_geometry` therefore reads the rect
        # through its own `_window_rect(hwnd)` seam instead of going through
        # `_user32()` directly, and that seam is what gets replaced here.
        with mock.patch.object(hostwin, "_window_rect",
                               return_value=(100, 100, 1380, 900)):
            self.assertEqual(hostwin.window_geometry("omni-farm3"),
                             (100, 100, 1280, 800))


class PresentingTheWindowAtSpawn(unittest.TestCase):
    """`present_qemu_window` -- the same restyle, plus a size and a show, run
    BEFORE anybody has seen the window.

    Order is the point: style, icon, SIZE, and only then show. Sizing after
    showing puts a 640x480 window on screen and yanks it to 1280x800 a frame
    later, which reads as a glitch in the product rather than as a window
    being set up.
    """

    def setUp(self):
        self.u = FakeUser32()
        self.shown = []
        self.patches = [
            mock.patch.object(hostwin, "backend",
                              return_value=hostwin.BACKEND_WIN32),
            mock.patch.object(hostwin, "find_window", return_value=4242),
            mock.patch.object(hostwin, "_user32", return_value=self.u),
            mock.patch.object(hostwin, "_window_rect",
                              return_value=(0, 0, 656, 519)),
            mock.patch.object(hostwin, "_client_size",
                              return_value=(640, 480)),
            mock.patch.object(hostwin, "apply_dwm_style", return_value={}),
            mock.patch.object(hostwin, "_show",
                              side_effect=lambda h, how:
                                  self.shown.append(how) or True),
        ]
        for p in self.patches:
            p.start()
        self.addCleanup(lambda: [p.stop() for p in self.patches])

    def test_it_sizes_the_CLIENT_area_to_the_panel_not_the_window(self):
        # THE BUG THIS EXISTS TO PREVENT. QEMU hands the guest the size of its
        # DRAWING AREA, so a window sized to 1280x800 gives the guest
        # 1280 - 16 x 800 - 39. The frame here is (656-640) x (519-480).
        hostwin.present_qemu_window("omni-farm3", panel=(1280, 800))
        self.assertIn((1280 + 16, 800 + 39),
                      [(p[2], p[3]) for p in self.u.positions])

    def test_it_shows_without_stealing_focus(self):
        # This fires DURING a launch the user started from the app and is
        # probably still looking at. A window that grabs the foreground
        # mid-boot is what makes people click Stop.
        hostwin.present_qemu_window("omni-farm3", panel=(1280, 800))
        self.assertEqual(self.shown, [hostwin.SW_SHOWNOACTIVATE])

    def test_it_keeps_the_window_resizable(self):
        hostwin.present_qemu_window("omni-farm3", panel=(1280, 800))
        self.assertTrue(self.u.style & hostwin.WS_THICKFRAME)

    def test_a_saved_geometry_beats_the_panel(self):
        # Where the user last left this window beats re-centring one they had
        # already placed. Geometry is a WINDOW rect (that is what
        # window_geometry returns), so it is applied as-is.
        hostwin.present_qemu_window("omni-farm3", panel=(1280, 800),
                                    geometry=(10, 20, 900, 600))
        self.assertIn((10, 20, 900, 600),
                      [p[:4] for p in self.u.positions])

    def test_a_missing_window_is_a_reason_not_an_exception(self):
        with mock.patch.object(hostwin, "find_window", return_value=None):
            result = hostwin.present_qemu_window("omni-gone")
        self.assertFalse(result["presented"])
        self.assertIn("no window", result["reason"].lower())

    def test_a_non_win32_backend_declines_with_a_reason(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_MACOS):
            result = hostwin.present_qemu_window("omni-farm3")
        self.assertFalse(result["presented"])
        self.assertIn("macos", result["reason"].lower())

    def test_a_failing_win32_call_is_a_reason_not_an_exception(self):
        # A window is never worth a boot: this runs inside spawn_qemu.
        with mock.patch.object(hostwin, "_user32",
                               side_effect=OSError("boom")):
            result = hostwin.present_qemu_window("omni-farm3",
                                                 panel=(1280, 800))
        self.assertFalse(result["presented"])
        self.assertNotEqual(result["reason"], "")


class ChromeElsewhere(unittest.TestCase):

    def test_a_non_win32_backend_declines_with_a_reason(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_MACOS):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertFalse(result["applied"])
        self.assertIn("macos", result["reason"].lower())


class ChromeOnLinuxIsDeferredAndSaysSo(unittest.TestCase):
    """Linux keeps QEMU's own frame until a host has verified a replacement.

    The reason has to name the state -- 'not implemented yet' -- rather than
    read as a failure, because nothing is broken: the window works, the guest
    renders on the GPU, and the VNC viewer is still there. Only the chrome is
    missing.
    """

    def test_an_x11_backend_declines_with_a_deferral_not_an_error(self):
        for name in (hostwin.BACKEND_XDOTOOL, hostwin.BACKEND_WMCTRL,
                     hostwin.BACKEND_XLIB):
            with mock.patch.object(hostwin, "backend", return_value=name):
                result = hostwin.apply_chrome("omni-farm3")
            self.assertFalse(result["applied"])
            self.assertIn("not implemented", result["reason"].lower())
            self.assertIn("linux", result["reason"].lower())

    def test_the_decline_never_raises_and_never_blocks_a_boot(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_XLIB):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertIsInstance(result, dict)
        self.assertIn("hwnd", result)


class FakeDwmapi:
    """Records DwmSetWindowAttribute calls, and can refuse the Windows 11
    attributes the way a Windows 10 host does (E_INVALIDARG)."""

    def __init__(self, refuse=()):
        self.calls = []
        self.refuse = set(refuse)

    def DwmSetWindowAttribute(self, hwnd, attribute, _data, _size):
        attr = attribute.value if hasattr(attribute, "value") else attribute
        self.calls.append((hwnd, attr))
        return 0x80070057 if attr in self.refuse else 0     # E_INVALIDARG


class TheDwmStylingTheSpecAsksFor(unittest.TestCase):
    """Design spec 3a/3b: QEMU's window gives up its whole caption, so the
    strip is the window that HAS one and "DWM styling (dark mode, rounded
    corners, border colour) therefore applies to the strip". None of it
    existed -- there was no DwmSetWindowAttribute anywhere in the tree, and
    what shipped as "restyled chrome" was caption + geometry only.
    """

    def _apply(self, dwm, **kw):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_WIN32), \
             mock.patch.object(hostwin, "_dwmapi", return_value=dwm):
            return hostwin.apply_dwm_style(11, **kw)

    def test_all_three_attributes_are_set_on_windows_11(self):
        dwm = FakeDwmapi()
        result = self._apply(dwm)
        self.assertEqual(result, {"dark": True, "rounded": True,
                                  "border": True})
        self.assertEqual([a for _h, a in dwm.calls],
                         [hostwin.DWMWA_USE_IMMERSIVE_DARK_MODE,
                          hostwin.DWMWA_WINDOW_CORNER_PREFERENCE,
                          hostwin.DWMWA_BORDER_COLOR])

    def test_the_attribute_numbers_are_the_documented_ones(self):
        # These ARE the API; a typo here is a silent no-op on real hardware.
        self.assertEqual(hostwin.DWMWA_USE_IMMERSIVE_DARK_MODE, 20)
        self.assertEqual(hostwin.DWMWA_WINDOW_CORNER_PREFERENCE, 33)
        self.assertEqual(hostwin.DWMWA_BORDER_COLOR, 34)
        self.assertEqual(hostwin.DWMWCP_ROUND, 2)

    def test_a_windows_10_host_still_gets_its_dark_caption(self):
        # 33 and 34 are Windows 11 (22000) only and come back E_INVALIDARG
        # there. Each attribute is set independently precisely so one the
        # running Windows does not know cannot take the others with it.
        dwm = FakeDwmapi(refuse=(hostwin.DWMWA_WINDOW_CORNER_PREFERENCE,
                                 hostwin.DWMWA_BORDER_COLOR))
        self.assertEqual(self._apply(dwm),
                         {"dark": True, "rounded": False, "border": False})

    def test_a_dwmapi_that_blows_up_is_three_falses_not_an_exception(self):
        class Boom:
            def DwmSetWindowAttribute(self, *_a):
                raise OSError("dwmapi.dll is not here")

        self.assertEqual(self._apply(Boom()),
                         {"dark": False, "rounded": False, "border": False})

    def test_a_non_windows_host_declines_without_touching_dwmapi(self):
        dwm = FakeDwmapi()
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_XDOTOOL), \
             mock.patch.object(hostwin, "_dwmapi", return_value=dwm):
            result = hostwin.apply_dwm_style(11)
        self.assertEqual(result, {"dark": False, "rounded": False,
                                  "border": False})
        self.assertEqual(dwm.calls, [])

    def test_the_border_colour_is_a_colorref_not_an_rgb(self):
        # COLORREF is 0x00BBGGRR. #2B2B2B is symmetric so the value cannot
        # catch a byte-order slip on its own -- what this pins is that it is
        # a plain int in COLORREF range rather than a string or a tuple.
        self.assertIsInstance(hostwin.BAR_BORDER_COLOR, int)
        self.assertTrue(0 <= hostwin.BAR_BORDER_COLOR <= 0x00FFFFFF)


class TheIconGoesThroughTheSameSeamAsEverythingElse(unittest.TestCase):
    """`_apply_icon` reached for `ctypes.windll.user32` directly for the
    LoadImageW half and used the injected `u` for the SendMessageW half, so
    a test could stand in for half the function and the other half went to
    the real Win32 API. One seam or none.

    THE ICON IS REAL NOW. `omnidroid/assets/omni-icon.png` is applied to
    QEMU's own window at spawn (`present_qemu_window`) and again by `view`;
    the PNG needs no conversion because `CreateIconFromResourceEx` takes PNG
    bytes directly. `LoadImageW` is still the path for a `.ico`, which is what
    this class pins.
    """

    def test_load_and_set_both_go_through_the_injected_user32(self):
        class FakeIconUser32(FakeUser32):
            def __init__(self):
                super().__init__()
                self.loaded = []

            def LoadImageW(self, inst, name, kind, cx, cy, flags):
                self.loaded.append((name, kind, flags, cx, cy))
                return 777

        u = FakeIconUser32()
        hostwin._apply_icon(u, 4242, "C:/omni.ico")
        # ONE LOAD PER SIZE, not one load reused for both. Windows asks a
        # window for a 16px icon (the caption) and a 32px one (Alt-Tab, the
        # taskbar); handing it the same handle for both leaves the shell
        # scaling one of them, which is the difference between "an app" and
        # "a script".
        self.assertEqual(len(u.loaded), 2)
        self.assertEqual({(cx, cy) for _n, _k, _f, cx, cy in u.loaded},
                         {(16, 16), (32, 32)})
        for name, kind, flags, _cx, _cy in u.loaded:
            self.assertEqual(name, "C:/omni.ico")
            self.assertEqual(kind, hostwin.IMAGE_ICON)
            self.assertTrue(flags & hostwin.LR_LOADFROMFILE)
        self.assertEqual(
            sorted(u.icons),
            sorted([(hostwin.WM_SETICON, hostwin.ICON_SMALL, 777),
                    (hostwin.WM_SETICON, hostwin.ICON_BIG, 777)]))

    def test_a_png_never_reaches_LoadImageW(self):
        """LoadImageW cannot read a PNG -- it wants a `.ico`, and this
        repository ships none. The asset it DOES ship is a 1024x1024 PNG, so
        sending it down the LoadImageW path would set no icon and report
        nothing. It goes to CreateIconFromResourceEx instead."""
        import tempfile
        from pathlib import Path
        png = Path(tempfile.mkdtemp()) / "omni-icon.png"
        png.write_bytes(b"\x89PNG\r\n\x1a\n" + b"\0" * 64)

        class TrackingUser32(FakeUser32):
            def __init__(self):
                super().__init__()
                self.loaded = []
                self.from_resource = 0

            def LoadImageW(self, *a):
                self.loaded.append(a)
                return 777

            @property
            def CreateIconFromResourceEx(self):
                outer = self

                class Fn:
                    restype = None
                    argtypes = None

                    def __call__(self, *_a):
                        outer.from_resource += 1
                        return 555
                return Fn()

        u = TrackingUser32()
        hostwin._apply_icon(u, 4242, str(png))
        self.assertEqual(u.loaded, [])
        self.assertEqual(u.from_resource, 2)      # one per size

    def test_the_shipped_asset_really_makes_an_icon(self):
        """Against the REAL Win32 call and the REAL file, because the whole
        point of the PNG route is that it works on this platform and the only
        way to know that is to ask it."""
        from omnidroid import qemu_proc
        icon = qemu_proc.window_icon_path()
        if icon is None:
            self.skipTest("no icon asset in this checkout")
        for cx, cy in ((16, 16), (32, 32)):
            handle = hostwin._load_icon(hostwin._user32(), icon, cx, cy)
            self.assertTrue(handle, f"no HICON at {cx}x{cy}")

    def test_an_icon_that_will_not_load_sets_nothing(self):
        class NoIconUser32(FakeUser32):
            def LoadImageW(self, *_a):
                return 0

        u = NoIconUser32()
        hostwin._apply_icon(u, 4242, "C:/missing.ico")
        self.assertEqual(u.icons, [])


if __name__ == "__main__":
    unittest.main()
