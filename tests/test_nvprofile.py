"""The NVIDIA per-app power profile (nvprofile.py): pure parts only. The
NVAPI call itself is exercised by booting on an NVIDIA host."""
import os
import unittest
from unittest import mock

from omnidroid import nvprofile


class Naming(unittest.TestCase):
    def test_profile_matches_on_the_exe_basename(self):
        self.assertEqual(nvprofile.exe_name(r"C:\x\y\qemu-system-x86_64.exe"),
                         "qemu-system-x86_64.exe")


class Describe(unittest.TestCase):
    def test_every_state_has_a_line(self):
        for st in ("set", "already", "skipped", "failed"):
            self.assertIn("nvidia", nvprofile.describe((st, "why")))

    def test_failure_and_skip_carry_the_reason(self):
        self.assertIn("why", nvprofile.describe(("failed", "why")))
        self.assertIn("why", nvprofile.describe(("skipped", "why")))


class NeverFailsALaunch(unittest.TestCase):
    def test_off_windows_is_a_skip_not_an_error(self):
        with mock.patch.object(nvprofile.sys, "platform", "linux"):
            self.assertEqual(nvprofile.apply("qemu")[0], "skipped")

    def test_kill_switch(self):
        with mock.patch.dict(os.environ, {"OMNI_NO_NVPROFILE": "1"}):
            self.assertEqual(nvprofile.apply("qemu")[0], "skipped")

    def test_no_driver_is_a_skip(self):
        with mock.patch.object(nvprofile._c, "WinDLL", side_effect=OSError("x"),
                               create=True):
            state, detail = nvprofile.apply("qemu")
            self.assertEqual(state, "skipped")
            self.assertIn("nvapi64", detail)


if __name__ == "__main__":
    unittest.main()
