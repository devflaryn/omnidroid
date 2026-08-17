#!/usr/bin/env python3
"""The window is on screen for the BOOT, and it keeps the guest's shape.

    python3 -m pytest tests/test_window_at_boot.py -q

Two behaviours, one window, and they are not independent.

**It appears immediately.** A gaming launch used to open a QEMU window, hide
it, boot for a minute with nothing on the user's screen, and then show the
window at the end -- so the first thing anyone ever saw was Roblox already
running, and the whole boot looked like the app had frozen. `place_window`
decides at spawn instead: a boot somebody is watching presents its window
straight away, at the panel size, and the loading animation is on screen while
Android comes up.

**It stays the right shape WHILE you drag it.** QEMU's GTK display hands the
guest the size of its drawing area and the guest re-modesets to match (ui/gtk.c
`gd_configure` -> `gd_set_ui_size`), while the gaming tune-up's `wm size`
override stays at the panel this boot was configured for. Drag the window to
900x900 and Android is laying out 1280x800 onto a square panel -- the squashed
picture. Holding the WINDOW at the panel's ratio keeps those two in agreement.

The correction is LIVE, not a snap when you let go. It runs at 8 ms against a
real modal drag and wins: measured, 2 of 246 samples off-ratio with it, 76 of
246 without. The guest is still only told once, a second after the drag stops,
because QEMU coalesces (`timer_mod(ui_timer, now + 1000)`, re-armed on every
change) and the lock stops correcting as soon as the shape is right.

The Win32 calls themselves are exercised against a real QEMU window in
tests/test_hostwin_backends.py's live checks; everything here is the policy
and the arithmetic, which are pure and must not need a window to test.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine, hostwin, qemu_proc

WINDOWED = ["qemu", "-display", "gtk,gl=on", "-device",
            "virtio-gpu-gl-pci,xres=1280,yres=800"]
HEADLESS = ["qemu", "-display", "none"]


def _clear_env(env=None):
    """The GPU policy reads three environment variables, and this repo's own
    dev shells set them. A test that inherits one is testing the shell."""
    for stale in ("OMNI_GPU", "OMNI_GL_WINDOW", "OMNI_NO_WINDOW",
                  "OMNI_HIDE_BOOT_WINDOW", "OMNI_DISPLAY"):
        if stale not in (env or {}):
            os.environ.pop(stale, None)


class WhoGetsToSeeTheirWindow(unittest.TestCase):
    """`window_shown_at_spawn` -- one line, and it is a product decision.

    Gaming is one instance a person started and is waiting on. Farming is
    fifty nobody is watching, and fifty windows appearing across the desktop
    is not a product. Both open a window (on Windows it is the only working GL
    context), and only one of them shows it.
    """

    def _shown(self, mode, cfg=None, env=None):
        with mock.patch.dict(os.environ, env or {}, clear=False):
            _clear_env(env)
            return qemu_proc.window_shown_at_spawn(cfg or {}, mode)

    def test_gaming_shows_its_window(self):
        self.assertTrue(self._shown(qemu_proc.MODES["gaming"]))

    def test_farming_does_not(self):
        self.assertFalse(self._shown(qemu_proc.MODES["farming"]))

    def test_a_config_can_ask_for_the_old_behaviour_back(self):
        # "Show me the boot" is a preference: somebody running a gaming
        # instance on a machine they are not looking at should not have to
        # take the window.
        self.assertFalse(self._shown(qemu_proc.MODES["gaming"],
                                     cfg={"qemu": {"hide_boot_window": True}}))

    def test_the_env_override_wins_over_the_config(self):
        self.assertFalse(self._shown(qemu_proc.MODES["gaming"],
                                     env={"OMNI_HIDE_BOOT_WINDOW": "1"}))

    def test_the_env_override_can_also_turn_the_config_off(self):
        # An env var that only ever narrows is a trap: `OMNI_HIDE_BOOT_WINDOW=0`
        # has to be able to overrule a config that says hide.
        self.assertTrue(self._shown(qemu_proc.MODES["gaming"],
                                    cfg={"qemu": {"hide_boot_window": True}},
                                    env={"OMNI_HIDE_BOOT_WINDOW": "0"}))


class ThePlacementHasExactlyThreeAnswers(unittest.TestCase):

    def _place(self, cmd, mode=None, env=None):
        seen = {}
        with mock.patch.dict(os.environ, env or {}, clear=False), \
             mock.patch.object(qemu_proc, "_present_window",
                               side_effect=lambda *a, **k: seen.update(
                                   present=True) or {"presented": True,
                                                     "client": (1280, 800)}), \
             mock.patch.object(qemu_proc, "_hide_window_if_wanted",
                               side_effect=lambda *a, **k: seen.update(
                                   hide=True) or True):
            _clear_env(env)
            return qemu_proc.place_window(cmd, "omni-u1", {}, mode=mode), seen

    def test_a_boot_with_no_window_places_nothing(self):
        placed, seen = self._place(HEADLESS, qemu_proc.MODES["gaming"])
        self.assertEqual(seen, {})
        self.assertFalse(placed["visible"])
        self.assertFalse(placed["hidden"])

    def test_a_watched_boot_is_presented(self):
        placed, seen = self._place(WINDOWED, qemu_proc.MODES["gaming"])
        self.assertTrue(seen.get("present"))
        self.assertNotIn("hide", seen)
        self.assertTrue(placed["visible"])
        self.assertEqual(placed["client"], (1280, 800))

    def test_an_unwatched_boot_is_hidden(self):
        placed, seen = self._place(WINDOWED, qemu_proc.MODES["farming"])
        self.assertTrue(seen.get("hide"))
        self.assertNotIn("present", seen)
        self.assertTrue(placed["hidden"])
        self.assertFalse(placed["visible"])

    def test_gpu_window_is_left_exactly_as_qemu_made_it(self):
        # The debugging hatch: "a visible native window, UNSTYLED -- for
        # debugging a GL problem with none of this code in the path". Neither
        # presenting nor hiding it is allowed to run.
        placed, seen = self._place(WINDOWED, qemu_proc.MODES["gaming"],
                                   env={"OMNI_GPU": "window"})
        self.assertEqual(seen, {})
        self.assertTrue(placed["visible"])
        self.assertFalse(placed["hidden"])


class QemuMustNotStretchTheGuest(unittest.TestCase):
    """`keep-aspect-ratio=on`, and why it is named rather than left to default.

    ui/gtk.c `gd_update_scale()` is the whole of it:

        if (keep_aspect_ratio) scale_x = scale_y = MIN(sx, sy);
        else                   scale_x = sx, scale_y = sy;

    With it off, every window that is not the guest's shape stretches the
    picture. The boot animation and the running game are not the same
    resolution, so on a window shown for the whole boot one of them is always
    being fitted into a window sized for the other.
    """

    def test_the_gtk_flags_carry_it(self):
        self.assertIn("keep-aspect-ratio=on", qemu_proc.window_flags("gtk"))

    def test_the_debug_window_still_gets_none_of_our_flags(self):
        self.assertEqual(
            qemu_proc.window_flags("gtk", policy=qemu_proc.GPU_WINDOW), "")

    def _display_is_accepted(self, display):
        """Whether the shipped QEMU parses this `-display` argument.

        `-M help` is the trick that makes this cheap: QEMU parses the whole
        command line before it acts on it, so a bad suboption is reported and
        the process exits non-zero, while a good one prints the machine list
        and exits 0 -- in well under a second, with no window and no guest.
        Booting one instead hangs the test run behind a GTK window that never
        closes, which is how this check was first written and why it is not
        written that way now.
        """
        import subprocess
        proc = subprocess.run([self._binary, "-display", display, "-M", "help"],
                              capture_output=True, text=True, timeout=30)
        return proc.returncode == 0, (proc.stderr or "") + (proc.stdout or "")

    def setUp(self):
        try:
            from omnidroid.config import qemu_bin
            binary = str(qemu_bin("qemu-system-x86_64"))
        except Exception as e:      # noqa: BLE001
            self.skipTest(f"no QEMU to ask: {e}")
        if not os.path.exists(binary):
            self.skipTest("no QEMU binary on this host")
        self._binary = binary

    def test_the_shipped_qemu_actually_accepts_it(self):
        """The suboption is ABSENT from `-display help`'s text (it lives in
        the QAPI schema, and that text is hand-maintained), and QEMU REFUSES
        an unknown suboption rather than ignoring it -- so getting this wrong
        does not cost the chrome, it costs every gaming boot. Ask the binary.
        """
        ok, output = self._display_is_accepted(
            "gtk,gl=on,zoom-to-fit=on,keep-aspect-ratio=on")
        self.assertTrue(ok, output)

    def test_the_probe_would_notice_an_option_this_qemu_lacks(self):
        """The control. Without it the test above passes on any binary that
        exits 0 for its own reasons, which is exactly the silent-no-op shape
        this repository keeps getting caught by."""
        ok, output = self._display_is_accepted(
            "gtk,gl=on,zoom-to-fit=on,no-such-suboption=on")
        self.assertFalse(ok)
        self.assertIn("no-such-suboption", output)


class TheSnapKeepsTheDimensionTheUserWasHolding(unittest.TestCase):
    """`aspect_fit`, which is the whole of the resize rule and is pure.

    WHICH dimension survives is the design. A user dragging the RIGHT edge
    changed the width and meant it, so the height follows; a user dragging the
    BOTTOM edge meant the height, so the width follows. Always solving for
    height would undo a bottom-edge drag completely -- a window that refuses
    to be resized.
    """

    RATIO = (1280, 800)      # 16:10

    def fit(self, w, h, **kw):
        return hostwin.aspect_fit(w, h, *self.RATIO, **kw)

    def test_a_right_edge_drag_keeps_the_new_width(self):
        self.assertEqual(self.fit(900, 800, previous=(1280, 800)), (900, 562))

    def test_a_bottom_edge_drag_keeps_the_new_height(self):
        self.assertEqual(self.fit(1280, 900, previous=(1280, 800)),
                         (1440, 900))

    def test_a_corner_drag_follows_whichever_moved_further(self):
        # Width moved 380, height moved 20: the corner was dragged sideways.
        self.assertEqual(self.fit(900, 820, previous=(1280, 800)), (900, 562))

    def test_a_correct_size_is_returned_unchanged(self):
        self.assertEqual(self.fit(1280, 800, previous=(1280, 800)),
                         (1280, 800))

    def test_it_never_goes_below_a_floor(self):
        # A drag toward zero must not hand the guest a 4x2 panel and a modeset
        # it will not come back from.
        w, h = self.fit(10, 10)
        self.assertGreaterEqual(w, 320)
        self.assertGreaterEqual(h, 200)

    def test_it_never_grows_past_the_screen(self):
        # Widening a 16:10 window by 700px asks for 437px more height. On a
        # window already low on the display that is a title bar the user can
        # no longer reach.
        w, h = self.fit(3000, 500, previous=(900, 562), maximum=(1920, 1040))
        self.assertLessEqual(w, 1920)
        self.assertLessEqual(h, 1040)
        # ...and the clamp must not itself distort: scaling one axis alone to
        # fit is the same bug this function exists to prevent.
        self.assertAlmostEqual(w / float(h), 1280 / 800.0, places=1)

    def test_widths_stay_even(self):
        # virtio-gpu's scanout assumes it; an odd width is a one-pixel tear
        # rather than an error, which is the worst kind of wrong.
        for size in ((999, 700), (1001, 300), (777, 777)):
            self.assertEqual(self.fit(*size)[0] % 2, 0)

    def test_a_degenerate_ratio_cannot_divide_by_zero(self):
        self.assertTrue(hostwin.aspect_fit(800, 600, 0, 0))


class TheLockKnowsWhenToDoNothing(unittest.TestCase):
    """`aspect_is_close` exists so the lock can say "nothing to do".

    Without it the watcher re-issues a SetWindowPos every poll for as long as
    the instance runs -- and each one re-arms QEMU's one-second ui_info timer,
    so the guest is told about a resize that never happened, forever.
    """

    def test_an_exact_ratio_is_close(self):
        self.assertTrue(hostwin.aspect_is_close((1280, 800), 1280, 800))

    def test_a_rounding_pixel_is_close(self):
        self.assertTrue(hostwin.aspect_is_close((1281, 800), 1280, 800))

    def test_a_real_mismatch_is_not(self):
        self.assertFalse(hostwin.aspect_is_close((1280, 900), 1280, 800))

    def test_the_correction_is_live_not_a_snap_on_release(self):
        """THE LOAD-BEARING NUMBERS.

        The lock corrects WHILE the window is being dragged. That works
        because a mouse move is milliseconds apart and the poll is faster:
        measured against a real modal drag, correcting every 8 ms left 2 of
        246 samples off-ratio, where correcting not at all left 76.

        The idle tier still has to be quick enough that the START of a drag is
        caught within a frame or two, or the lock reads as kicking in late.

        And both have to stay well inside QEMU's own one-second `ui_info`
        debounce (`timer_mod(ui_timer, now + 1000)`, re-armed on every
        change), so the guest is told about the shape ONCE, after it is
        already correct -- never dragged through a modeset per frame."""
        self.assertLessEqual(hostwin.ASPECT_POLL_SECONDS, 0.010)
        self.assertLessEqual(hostwin.ASPECT_IDLE_POLL_SECONDS, 0.035)
        self.assertLess(hostwin.ASPECT_IDLE_SECONDS, 1.0)
        self.assertLessEqual(hostwin.ASPECT_POLL_SECONDS,
                             hostwin.ASPECT_IDLE_POLL_SECONDS)

    def test_the_axis_is_latched_rather_than_re_decided(self):
        """A bottom-edge drag means the HEIGHT, for the whole drag. Re-deciding
        per frame flips the answer as soon as our own correction has moved the
        width, and then the lock and the drag argue."""
        # Height latched: the width follows, however the pair currently look.
        self.assertEqual(
            hostwin.aspect_fit(880, 790, 1280, 800, axis=hostwin.AXIS_HEIGHT),
            (1264, 790))
        # Width latched, same input: the other answer.
        self.assertEqual(
            hostwin.aspect_fit(880, 790, 1280, 800, axis=hostwin.AXIS_WIDTH),
            (880, 550))


class TheLockIsOnlyStartedWhenThereIsSomethingToHold(unittest.TestCase):

    def _start(self, run, panel=(1280, 800)):
        spawned = []
        with mock.patch.object(engine, "_run_record", return_value=run), \
             mock.patch.object(engine, "boot_gl_panel", return_value=panel), \
             mock.patch.object(engine, "_running_window_lock_pid",
                               return_value=None), \
             mock.patch.object(engine, "_write_window_lock_pid"), \
             mock.patch.object(engine, "_spawn_window_lock",
                               side_effect=lambda *a, **k: spawned.append(a)
                               or mock.Mock(pid=4321)):
            pid = engine.maybe_start_window_lock({"name": "u1"}, "u1")
        return pid, spawned

    def test_a_presented_window_is_held(self):
        pid, spawned = self._start({"native_window": True,
                                    "window_visible": True,
                                    "identity": "omni-u1", "pid": 42})
        self.assertEqual(pid, 4321)
        self.assertEqual(spawned[0], ("u1", "omni-u1", 42, (1280, 800)))

    def test_a_hidden_window_is_not(self):
        # Farming: a window nobody can see does not need holding, and fifty
        # instances would be fifty processes spent on nothing.
        pid, spawned = self._start({"native_window": True,
                                    "window_visible": False})
        self.assertIsNone(pid)
        self.assertEqual(spawned, [])

    def test_a_software_boot_with_no_panel_is_not(self):
        pid, spawned = self._start({"native_window": True,
                                    "window_visible": True}, panel=None)
        self.assertIsNone(pid)
        self.assertEqual(spawned, [])

    def test_a_second_launch_does_not_stack_a_second_lock(self):
        with mock.patch.object(engine, "_run_record",
                               return_value={"native_window": True,
                                             "window_visible": True}), \
             mock.patch.object(engine, "boot_gl_panel",
                               return_value=(1280, 800)), \
             mock.patch.object(engine, "_running_window_lock_pid",
                               return_value=9999), \
             mock.patch.object(engine, "_spawn_window_lock") as spawn:
            self.assertEqual(engine._ensure_window_lock("u1"), 9999)
        spawn.assert_not_called()


class TheWindowCarriesOurNameAndOurIcon(unittest.TestCase):
    """QEMU calls its window `QEMU (omni-<account>)` and gives it the QEMU
    logo. Both are replaced from outside, on QEMU's own window, with no
    patched build and no second window stacked on top to caption it --
    `WM_SETTEXT` and `WM_SETICON` are both marshalled between processes.
    """

    def test_the_title_is_ours_and_names_the_account(self):
        self.assertEqual(qemu_proc.window_title("HezMi_ImYu"),
                         "omni: HezMi_ImYu")

    def test_the_icon_asset_is_inside_the_package(self):
        # PyInstaller's collect_data_files only picks up data that lives in
        # the package, so a repo-root assets/ dir vanishes from the frozen
        # build and the window silently falls back to QEMU's own logo.
        icon = qemu_proc.window_icon_path()
        self.assertIsNotNone(icon, "the shipped icon asset is missing")
        self.assertTrue(icon.is_file())
        self.assertEqual(icon.parent.name, "assets")
        self.assertEqual(icon.parent.parent.name, "omnidroid")

    def test_apply_identity_reports_each_half_separately(self):
        # "The title landed but the icon did not" is a real outcome (a
        # missing asset in a frozen build), and one blended boolean cannot
        # say it.
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_WIN32), \
             mock.patch.object(hostwin, "_apply_title"), \
             mock.patch.object(hostwin, "_apply_icon"), \
             mock.patch.object(hostwin, "_window_title",
                               return_value="omni: u1"), \
             mock.patch.object(hostwin, "_user32") as u:
            u.return_value.SendMessageW.side_effect = [11, 11]   # icon unchanged
            result = hostwin.apply_identity(4242, title="omni: u1",
                                            icon="x.png")
        self.assertTrue(result["title"])
        self.assertFalse(result["icon"])

    def test_it_never_raises_on_a_backend_that_cannot(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_MACOS):
            self.assertEqual(hostwin.apply_identity(1, title="t", icon="i"),
                             {"title": False, "icon": False})


class RenamingTheWindowMustNotLoseIt(unittest.TestCase):
    """THE DEFECT THIS EXISTS FOR. `find_window` matches the identity as a
    TITLE SUBSTRING, and the identity is `omni-<account>` while our title is
    `omni: <account>` -- so the moment the window was renamed, an ANDed
    title+pid search matched nothing and `view` reported the window DESTROYED
    on an instance whose window was on screen in front of the user.

    The pid is the stronger claim of the two; the title is a string that both
    QEMU and this module write.
    """

    # (hwnd, title, pid, visible, client, class). The last two are what tell
    # a real guest window from the half-dozen a QEMU process owns -- see
    # hostwin._is_plausible_guest_window.
    GUEST = "gdkWindowToplevel"
    REAL = (1, "omni: farm3", 4242, True, (1280, 800), GUEST)
    STOCK = (2, "QEMU (omni-farm3)", 4242, True, (640, 505), GUEST)
    PBUFFER = (3, "NVOGLDC invisible", 4242, False, (1914, 994),
               "NVOpenGLPbuffer")
    GDIPLUS = (4, "GDI+ Window (qemu-system-x86_64.exe)", 4242, False, (1, 1),
               "GDI+ Hook Window Class")
    IME = (5, "Default IME", 4242, False, (0, 0), "IME")

    def _find(self, table, match, pid):
        by_hwnd = {t[0]: t[1:] for t in table}
        patches = (
            mock.patch.object(hostwin, "_walk_windows",
                              side_effect=lambda visit: [visit(t[0])
                                                         for t in table]),
            mock.patch.object(hostwin, "_window_title",
                              side_effect=lambda h: by_hwnd[h][0]),
            mock.patch.object(hostwin, "_window_pid",
                              side_effect=lambda h: by_hwnd[h][1]),
            mock.patch.object(hostwin, "_client_size",
                              side_effect=lambda h: by_hwnd[h][3]),
            mock.patch.object(hostwin, "_window_class",
                              side_effect=lambda h: by_hwnd[h][4]),
            mock.patch.object(
                hostwin, "_user32",
                return_value=mock.Mock(
                    IsWindowVisible=lambda h: by_hwnd[h][2],
                    GetAncestor=lambda h, _flag: h)),
            mock.patch.object(hostwin, "backend",
                              return_value=hostwin.BACKEND_WIN32),
        )
        for patch in patches:
            patch.start()
        try:
            return hostwin._enum_windows(match, pid)
        finally:
            for patch in patches:
                patch.stop()

    def test_a_renamed_window_is_still_found_by_pid(self):
        found = self._find([self.REAL, self.PBUFFER, self.IME],
                           "omni-farm3", 4242)
        self.assertEqual([h for h, _t in found], [1])

    def test_the_decoys_a_qemu_process_owns_are_never_returned(self):
        """MEASURED at product timing: `find_window` runs ~50 ms after spawn,
        QEMU's real window does not exist until t+0.22 s, and a pid-only
        search returned the NVIDIA driver's invisible pbuffer. Everything
        downstream then styled, renamed, resized and watched a window nothing
        is ever drawn in, while the real one sat on the user's screen wearing
        QEMU's own name. Returning NOTHING is what puts find_window back to
        WAITING for the real window."""
        self.assertEqual(
            self._find([self.PBUFFER, self.GDIPLUS, self.IME],
                       "omni-farm3", 4242),
            [])

    def test_a_stock_titled_window_still_wins_over_a_decoy(self):
        # The title is still a signal, just no longer the only one.
        found = self._find([self.PBUFFER, self.STOCK], "omni-farm3", 4242)
        self.assertEqual(found[0][0], 2)

    def test_the_visible_one_wins_over_a_hidden_stock_titled_one(self):
        # Renamed-and-visible beats stock-titled-and-hidden: what is on the
        # user's screen is the window they mean. This is the case GTK
        # actually produces -- it keeps a hidden helper top-level that still
        # carries QEMU's original title.
        hidden_stock = (2, "QEMU (omni-farm3)", 4242, False, (640, 505),
                        self.GUEST)
        found = self._find([hidden_stock, self.REAL], "omni-farm3", 4242)
        self.assertEqual(found[0][0], 1)

    def test_a_title_search_with_no_pid_still_only_matches_the_title(self):
        # Without a pid there is nothing to fall back TO, and returning every
        # window on the desktop would be worse than returning none.
        self.assertEqual(self._find([self.REAL], "omni-farm3", None), [])

    def test_another_process_is_never_returned(self):
        other = (1, "omni: farm3", 999, True, (1280, 800), self.GUEST)
        self.assertEqual(self._find([other], "omni-farm3", 4242), [])


class ShowAndHideBothWriteThemselvesDown(unittest.TestCase):
    """`window_visible` is the ONE field in run.json that is not a property of
    the argv -- it is the live state of a window, and it changes while the
    instance runs. The app's View/Hide button is drawn off it, so a hide that
    forgets to record itself leaves the button offering to hide an
    already-hidden window, and doing nothing.

    Against a REAL run.json, because the recorder deliberately refuses to
    create one: `_write_run_record` makes the directory it writes into, and a
    fresh record carrying nothing but this flag is read as a live instance by
    `running_instances` and `reconcile_runtime`.
    """

    def setUp(self):
        import json
        import shutil
        import tempfile
        from pathlib import Path
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: shutil.rmtree(self.tmp, ignore_errors=True))
        (self.tmp / "run.json").write_text(json.dumps(
            {"identity": "omni-u1", "pid": 42, "native_window": True,
             "gpu": "gl", "window_visible": True}))

    def _read(self):
        import json
        return json.loads((self.tmp / "run.json").read_text())

    def test_hiding_records_it(self):
        with mock.patch.object(engine, "runtime_dir", return_value=self.tmp), \
             mock.patch.object(engine, "running_pid", return_value=42), \
             mock.patch.object(engine, "load_config", return_value={}), \
             mock.patch.object(engine, "load_account",
                               return_value={"name": "u1",
                                             "vnc_port": 18001}), \
             mock.patch.object(engine, "_running_window_bar_pid",
                               return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        return_value=True):
            engine.cmd_view(type("A", (), {"name": "u1", "hide": True,
                                           "json": False, "start": False,
                                           "native": False, "mode": None,
                                           "debug": False, "offset": None,
                                           "timeout": 60})())
        self.assertFalse(self._read()["window_visible"])

    def test_a_hide_that_did_not_happen_is_not_recorded_as_one(self):
        # hide_qemu_window's contract is to report False rather than raise
        # when the window is gone. Writing "hidden" anyway would make the app
        # offer View for a window that no longer exists.
        with mock.patch.object(engine, "runtime_dir", return_value=self.tmp), \
             mock.patch.object(engine, "running_pid", return_value=42), \
             mock.patch.object(engine, "load_config", return_value={}), \
             mock.patch.object(engine, "load_account",
                               return_value={"name": "u1",
                                             "vnc_port": 18001}), \
             mock.patch.object(engine, "_running_window_bar_pid",
                               return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        return_value=False):
            engine.cmd_view(type("A", (), {"name": "u1", "hide": True,
                                           "json": False, "start": False,
                                           "native": False, "mode": None,
                                           "debug": False, "offset": None,
                                           "timeout": 60})())
        self.assertTrue(self._read()["window_visible"])

    def test_the_recorder_never_creates_a_record(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)
        with mock.patch.object(engine, "runtime_dir", return_value=self.tmp):
            engine._record_window_visible("u1", False)
            engine._record_window_client("u1", (1280, 800))
        self.assertFalse(self.tmp.exists())


class TheAppCanTellWhereThePixelsAre(unittest.TestCase):
    """`account_status` is the bridge: the app draws its View/Hide button and
    its status line off these fields, and before they existed it printed a
    `vnc_port` that nothing was listening on."""

    def _status(self, run):
        with mock.patch.object(engine, "running_pid", return_value=42), \
             mock.patch.object(engine, "pool") as p, \
             mock.patch("pathlib.Path.read_text",
                        return_value=__import__("json").dumps(run)):
            p.is_slot.return_value = False
            return engine.account_status({"name": "u1", "base": "x86",
                                          "vnc_port": 18001,
                                          "adb_port": 15001})

    def test_a_gl_window_boot_reports_no_vnc(self):
        rec = self._status({"native_window": True, "gpu": "gl",
                            "window_visible": True,
                            "window_client": [1280, 800]})
        self.assertFalse(rec["has_vnc"])
        self.assertTrue(rec["window_visible"])
        self.assertEqual(rec["window_client"], [1280, 800])

    def test_a_software_boot_reports_its_vnc(self):
        rec = self._status({"native_window": False, "gpu": "software"})
        self.assertTrue(rec["has_vnc"])
        self.assertFalse(rec["window_visible"])


if __name__ == "__main__":
    unittest.main()
