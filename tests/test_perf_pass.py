#!/usr/bin/env python3
"""The 2026-09-01 performance pass: file-backed guest RAM, free-page
reporting, a host-aware panel, and virtio input.

    python3 tests/test_perf_pass.py

Every class here pins a number that was MEASURED on the Windows box
(i7-13700F, RTX 4060, QEMU 11.1.0-omni, Bliss 16.9.7 x86_64, PS99) rather
than reasoned about, and says which one.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import qemu_proc  # noqa: E402


class RamFileBacking(unittest.TestCase):
    """Guest RAM comes from a file we own, not from the system pagefile.

    The whole point is the COMMIT CHARGE. `-m` is charged 1:1 against the
    Windows commit limit when the allocation is private and not at all when
    it is a mapped file -- probed at +3078 MB vs +12 MB for 3 GiB of guest
    RAM before qemu-patches/0007 was written. Commit, not RAM, is what caps
    this host's farming fleet (`doctor` -> capacity_farming), so this is the
    wall moving rather than a tweak.
    """

    def _mode(self, profile="density", mem=3072):
        return {"profile": profile, "mem": mem}

    def test_density_on_windows_gets_the_scratch_dir(self):
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=True, supported=True)
        self.assertEqual(env[qemu_proc.RAM_FILE_ENV],
                         str(qemu_proc.scratch_dir({})))

    def test_performance_profile_is_left_alone(self):
        """Gaming follows once density has proven it, not before. A gaming
        instance is one instance; it is not what the commit limit is about,
        and a soft fault in the middle of a frame is."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode("performance"),
                                     is_windows=True, supported=True)
        self.assertNotIn(qemu_proc.RAM_FILE_ENV, env)

    def test_not_on_linux_or_macos(self):
        """Those hosts have madvise and a balloon that decommits for real.
        This buys them nothing and would add a file to reap."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=False, supported=True)
        self.assertNotIn(qemu_proc.RAM_FILE_ENV, env)

    def test_a_qemu_without_the_patch_is_not_asked(self):
        """A stock binary ignores the variable, so setting it is harmless --
        but then nothing has changed and the planner must not believe the
        commit is gone. The capability gate is what the planner reads."""
        env = qemu_proc.ram_file_env({}, cfg={}, mode=self._mode(),
                                     is_windows=True, supported=False)
        self.assertNotIn(qemu_proc.RAM_FILE_ENV, env)

    def test_the_disk_gate_budgets_the_worst_case(self):
        """Exhausting the disk used to fail a LAUNCH. With guest RAM in a
        sparse file it kills a RUNNING guest -- STATUS_IN_PAGE_ERROR on a
        mapped view, no clean error path -- so the reserve is `-m` itself,
        not the file's expected allocation."""
        self.assertEqual(
            qemu_proc.ram_file_reserve_mb(self._mode(mem=4096), cfg={},
                                          supported=True, is_windows=True),
            4096)

    def test_nothing_is_reserved_when_the_file_is_not_in_play(self):
        for kwargs in ({"supported": False, "is_windows": True},
                       {"supported": True, "is_windows": False}):
            self.assertEqual(
                qemu_proc.ram_file_reserve_mb(self._mode(), cfg={}, **kwargs),
                0)
        self.assertEqual(
            qemu_proc.ram_file_reserve_mb(self._mode("performance"), cfg={},
                                          supported=True, is_windows=True), 0)

    def test_the_capability_probe_reads_the_pkgversion_suffix(self):
        """tools/build_qemu.py derives --with-pkgversion from the series it
        actually applied, so the parenthesised suffix -- not the version
        number, which the patches do not bump -- is the honest answer."""
        qemu_proc._omni_caps_of.cache_clear()
        with mock.patch.object(qemu_proc.subprocess, "run") as run:
            run.return_value = mock.Mock(stdout=(
                "QEMU emulator version 11.1.0 "
                "(omni-window+omni-ram-file+omni-punch-hole)\n"))
            caps = qemu_proc._omni_caps_of("qemu-system-x86_64")
        self.assertEqual(caps,
                         ("omni-window", "omni-ram-file", "omni-punch-hole"))
        qemu_proc._omni_caps_of.cache_clear()

    def test_a_stock_qemu_advertises_nothing(self):
        qemu_proc._omni_caps_of.cache_clear()
        with mock.patch.object(qemu_proc.subprocess, "run") as run:
            run.return_value = mock.Mock(stdout=(
                "QEMU emulator version 11.0.50 (v11.0.0-12631-g54e84cdc7a)\n"))
            self.assertEqual(qemu_proc._omni_caps_of("q"), ())
        qemu_proc._omni_caps_of.cache_clear()


class FreePageReportingFollowsTheBinary(unittest.TestCase):
    """`free-page-reporting=on` was dropped on Windows because the SHIPPED
    QEMU could not honour it -- 925 failed discards a minute, zero pages
    reclaimed. That was a property of the binary, and the product does not
    ship that binary any more (patches 0005 and 0008). So the flag follows
    the capability, not the platform."""

    def _args(self, windows, punch):
        with mock.patch.object(qemu_proc, "IS_WINDOWS", windows), \
             mock.patch.object(qemu_proc, "qemu_supports_punch_hole",
                               return_value=punch):
            return " ".join(qemu_proc.balloon_device({}, {}))

    def test_a_patched_windows_qemu_reports_free_pages(self):
        self.assertIn("free-page-reporting=on", self._args(True, True))

    def test_a_stock_windows_qemu_still_does_not(self):
        self.assertNotIn("free-page-reporting", self._args(True, False))

    def test_the_balloon_itself_is_always_attached(self):
        """apply_balloon_target's QMP inflate has to have a device to talk
        to on every host, patched or not."""
        for windows, punch in ((True, True), (True, False), (False, False)):
            self.assertIn("virtio-balloon-pci", self._args(windows, punch))

    def test_linux_and_macos_are_untouched(self):
        self.assertIn("free-page-reporting=on", self._args(False, False))


class HostAwarePanel(unittest.TestCase):
    """Gaming's panel grows to the host's screen, up to PERF_PANEL_CEIL.

    The base was never pinned to 1280x800 -- there is no `video=` on its
    kernel command line and its DRM connector already lists 1920x1440 and
    3840x2160. Measured on this box: 1080p and 1440p both come up at exactly
    what they were asked for (`wm size` read back 1920x1080 / 2560x1440),
    boot 35.6 s against 800p's 33.6 s.
    """

    def test_a_1440p_screen_gets_the_1080p_ceiling(self):
        self.assertEqual(
            qemu_proc.host_panel((1280, 800), screen=(2560, 1440)),
            qemu_proc.PERF_PANEL_CEIL)

    def test_a_1080p_screen_keeps_the_smaller_panel(self):
        """The gaming window is a REAL window at exactly this size, so a
        1920x1080 panel on a 1920x1080 desktop is a window whose caption is
        off the top of the screen. HOST_PANEL_MARGIN is what refuses it."""
        self.assertEqual(
            qemu_proc.host_panel((1280, 800), screen=(1920, 1080)),
            (1280, 800))

    def test_an_unreadable_screen_costs_the_upgrade_not_the_boot(self):
        with mock.patch.object(qemu_proc, "host_screen_size",
                               return_value=None):
            self.assertEqual(qemu_proc.host_panel((1280, 800)), (1280, 800))

    def test_a_bigger_declared_panel_is_never_shrunk(self):
        self.assertEqual(
            qemu_proc.host_panel((2560, 1440), screen=(3840, 2160)),
            (2560, 1440))

    def test_an_explicit_panel_still_wins_outright(self):
        """`--panel 800p` has to remain the way to buy frames back."""
        with mock.patch.dict(os.environ, {"OMNI_PANEL": "1280x800"}), \
             mock.patch.object(qemu_proc, "host_screen_size",
                               return_value=(3840, 2160)):
            self.assertEqual(
                qemu_proc.panel_for({"profile": "performance",
                                     "panel": (1280, 800)}, {}),
                (1280, 800))

    def test_density_is_never_grown(self):
        """Farming's 640x480 panel is the point of farming."""
        env = {k: v for k, v in os.environ.items() if k != "OMNI_PANEL"}
        with mock.patch.dict(os.environ, env, clear=True), \
             mock.patch.object(qemu_proc, "host_screen_size",
                               return_value=(3840, 2160)):
            self.assertEqual(
                qemu_proc.panel_for({"profile": "density",
                                     "panel": (640, 480)}, {}),
                (640, 480))


class InputIsVirtioFirst(unittest.TestCase):
    """A USB HID device is POLLED -- QEMU's usb-hid advertises a 10 ms
    interrupt interval. Measured host->guest with 40 events sent 4 ms apart
    and read back off the guest's own /dev/input node:

        usb-tablet      p50 3.84 ms   p90 5.31 ms   max 9.53 ms
        virtio-tablet   p50 4.01 ms   p90 6.02 ms   max 6.20 ms

    Same median, and the tail -- which is what input lag actually feels
    like -- loses the 10 ms polling ceiling.
    """

    def _args(self, policy, arm=False):
        with mock.patch.dict(os.environ, {"OMNI_INPUT": policy}):
            return " ".join(qemu_proc.usb_devices({}, arm=arm))

    def test_virtio_is_named_before_usb(self):
        """qemu_input_find_handler takes the FIRST handler whose mask covers
        the event, and handlers register in command-line order. Verified on
        hardware: QMP input-send-event with x=5000/25000/12000 landed on the
        guest's 'QEMU Virtio Tablet' node and nothing reached the USB one."""
        args = self._args("virtio")
        self.assertLess(args.index("virtio-tablet-pci"),
                        args.index("usb-tablet"))

    def test_usb_stays_attached_as_the_fallback(self):
        """If virtio_input does not bind in some future guest, the USB pair
        behind it is still a real mouse and keyboard -- so the worst case of
        the faster device is the old behaviour, not an instance nobody can
        click on (see MODES['farming']'s `usb` note for what that costs)."""
        args = self._args("virtio")
        for dev in ("qemu-xhci", "usb-kbd", "usb-tablet"):
            self.assertIn(dev, args)

    def test_the_old_path_is_one_env_var_away(self):
        args = self._args("usb")
        self.assertNotIn("virtio-tablet-pci", args)
        self.assertIn("usb-tablet", args)

    def test_arm_keeps_its_own_controller_and_gets_virtio_too(self):
        args = self._args("virtio", arm=True)
        self.assertIn("nec-usb-xhci", args)
        self.assertLess(args.index("virtio-tablet-pci"),
                        args.index("usb-tablet"))

    def test_a_mode_with_no_hands_still_gets_nothing(self):
        self.assertEqual(qemu_proc.usb_devices({"usb": False}, arm=False), [])
        self.assertEqual(qemu_proc.usb_devices({"usb": False}, arm=True), [])


if __name__ == "__main__":
    unittest.main()
