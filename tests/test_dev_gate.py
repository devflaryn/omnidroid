#!/usr/bin/env python3
"""Debug is a per-BOOT option, not a base and not an account property.

There is no dev base and no dev-mode gate: every registered base is dual-use.
`omni start --debug` (or OMNI_DEBUG_BOOT=1) attaches the devkit disk as vdc;
a plain boot never gets it, so a production instance's hardware profile is
unchanged.

    python3 tests/test_dev_gate.py     (or: pytest tests/)
"""
import os
import sys
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine as omni  # noqa: E402
from omnidroid import bases as b  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402


class DebugBootRequest(unittest.TestCase):
    """_debug_boot_requested: --debug or OMNI_DEBUG_BOOT, never implied."""

    def setUp(self):
        self._saved = os.environ.pop(b.DEBUG_ENV, None)

    def tearDown(self):
        os.environ.pop(b.DEBUG_ENV, None)
        if self._saved is not None:
            os.environ[b.DEBUG_ENV] = self._saved

    def test_flag_requests_debug(self):
        self.assertTrue(b._debug_boot_requested(SimpleNamespace(debug=True)))

    def test_default_is_production(self):
        self.assertFalse(b._debug_boot_requested(SimpleNamespace(debug=False)))
        self.assertFalse(b._debug_boot_requested(SimpleNamespace()))

    def test_env_enables_debug(self):
        os.environ[b.DEBUG_ENV] = "1"
        self.assertTrue(b._debug_boot_requested(SimpleNamespace(debug=False)))

    def test_only_truthy_env_enables(self):
        for v in ("0", "", "no", "off"):
            os.environ[b.DEBUG_ENV] = v
            self.assertFalse(b._debug_boot_requested(SimpleNamespace(debug=False)),
                             f"{v!r} should not enable debug")


class DevkitAttachment(unittest.TestCase):
    """devkit_drive_args attaches vdc only on a debug boot, and only when the
    per-arch devkit disk exists."""

    def _args(self, debug, disk_exists):
        acct = {"name": "u1", "base": "arm", "ephemeral": True}
        cfg = {"images_dir": "/img",
               "bases": {"arm": {"type": "arm-uefi"}}}
        with mock.patch.object(qemu_proc, "devkit_disk_for_base",
                               return_value=("/img/base_arm_devkit.qcow2"
                                             if disk_exists else None)), \
             mock.patch("pathlib.Path.exists", return_value=disk_exists):
            return qemu_proc.devkit_drive_args(acct, cfg, debug,
                                               ",snapshot=on")

    def test_production_boot_has_no_vdc(self):
        self.assertEqual(self._args(debug=False, disk_exists=True), [])

    def test_debug_boot_attaches_vdc(self):
        args = self._args(debug=True, disk_exists=True)
        self.assertIn("virtio-blk-pci,drive=vdc", args)
        self.assertTrue(any("vdc" in a for a in args))

    def test_debug_boot_without_disk_is_a_noop(self):
        self.assertEqual(self._args(debug=True, disk_exists=False), [])


class NoDevBaseSymbols(unittest.TestCase):
    """The dev-base gate is gone; its symbols must not resurface."""

    def test_removed_symbols_are_absent(self):
        for name in ("acct_is_dev", "base_is_dev", "assert_dev_allowed",
                     "visible_bases", "dev_mode_enabled", "DEV_BASE_TAG",
                     "build_dev_base"):
            self.assertFalse(hasattr(omni, name),
                             f"engine.{name} should have been removed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
