"""Ephemeral x86 boot: the product launch path on a Windows/amd64 host.

The x86 branch of qemu_command() only ever built the NON-ephemeral form --
per-account system.qcow2/data.qcow2 overlays -- so it had no way to express
"boot the shared base template snapshot=on, with THIS baked Roblox version as
/data". Ephemeral is the product's launch model (build_acct writes no
overlays at all), so on x86 the offset a launch resolved was simply ignored
and the instance booted whatever was in the account dir.

Mirrors tests/test_ephemeral_boot.py, which covers the same rules for arm.
"""
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402


def _cfg():
    return {
        "images_dir": "/imgs",
        "data_template": "x86/data-template-8g.qcow2",
        "qemu": {"smp": 4, "mem_mb": 4096},
        "bases": {
            "x86": {"type": "x86-bliss", "disk": "x86/base_x86.qcow2",
                    "kernel": "x86/base_x86.kernel",
                    "initrd": "x86/base_x86.initrd.img",
                    "src": "/android-2024-10-11"},
        },
    }


def _acct(**over):
    a = {"name": "u1", "base": "x86", "adb_port": 16001, "qmp_port": 17001,
         "vnc_port": 18001}
    a.update(over)
    return a


def _drives(cmd):
    return [cmd[i + 1] for i, a in enumerate(cmd) if a == "-drive"]


class EphemeralX86Boot(unittest.TestCase):
    def _cmd(self, acct, interactive=False):
        with mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda x: x), \
             mock.patch.object(qemu_proc, "default_accel", return_value="whpx"), \
             mock.patch.object(qemu_proc, "resolve_mode",
                               return_value={"smp": 4, "mem": 4096,
                                             "name": "playable"}), \
             mock.patch.object(qemu_proc, "_assert_port_triple", return_value=1), \
             mock.patch.object(qemu_proc, "resolve_gpu_display",
                               return_value=([], ["-display", "none"])), \
             mock.patch.object(qemu_proc, "usb_devices", return_value=[]), \
             mock.patch.object(qemu_proc, "balloon_device", return_value=[]), \
             mock.patch.object(qemu_proc, "devkit_drive_args", return_value=[]), \
             mock.patch("omnidroid.engine.runtime_dir",
                        side_effect=lambda n: Path(f"/RT/{n}")), \
             mock.patch("omnidroid.engine.account_dir",
                        side_effect=lambda n: Path(f"/ACC/{n}")):
            return qemu_proc.qemu_command(acct, _cfg(), interactive)

    def test_an_ephemeral_boot_opens_the_shared_base_snapshot_on(self):
        cmd = self._cmd(_acct(ephemeral=True))
        drives = _drives(cmd)
        self.assertIn("/imgs/x86/base_x86.qcow2", drives[0].replace("\\", "/"))
        self.assertTrue(all("snapshot=on" in d for d in drives[:2]),
                        f"both disks must be throwaway: {drives[:2]}")

    def test_the_resolved_offset_becomes_the_data_disk(self):
        # WHICH Roblox this boot runs is decided here and nowhere else.
        off = "x86/base_x86_data_offset_arceusremote.qcow2"
        cmd = self._cmd(_acct(ephemeral=True, data_image=off))
        data = _drives(cmd)[1].replace("\\", "/")
        self.assertIn(off, data)
        self.assertIn("snapshot=on", data)

    def test_no_offset_falls_back_to_the_shared_data_template(self):
        # An x86 base has no base["data"]; a game-less boot gets the empty
        # template (the `--offset none` / `--apk` paths).
        cmd = self._cmd(_acct(ephemeral=True))
        data = _drives(cmd)[1].replace("\\", "/")
        self.assertIn("x86/data-template-8g.qcow2", data)

    def test_a_non_ephemeral_boot_still_uses_per_account_overlays(self):
        cmd = self._cmd(_acct())
        drives = [d.replace("\\", "/") for d in _drives(cmd)]
        self.assertIn("/ACC/u1/system.qcow2", drives[0])
        self.assertIn("/ACC/u1/data.qcow2", drives[1])
        self.assertFalse(any("snapshot=on" in d for d in drives[:2]))

    def test_it_still_boots_by_kernel_and_initrd(self):
        cmd = self._cmd(_acct(ephemeral=True))
        self.assertIn("-kernel", cmd)
        self.assertIn("-initrd", cmd)
        # x86 has no UEFI: an efivars/pflash argument here would be a bug.
        self.assertNotIn("-pflash", cmd)


class LaunchHandleIsHostArch(unittest.TestCase):
    """build_acct() pinned arch="arm", so every x86 launch died with
    'no arm base registered' before it allocated anything."""

    def test_an_x86_base_is_an_acceptable_instance_base(self):
        cfg = _cfg()
        cfg["current_base"] = "x86"
        tag = omni._select_base_tag(cfg)
        self.assertEqual(tag, "x86")
        self.assertEqual(omni.base_type(cfg["bases"][tag]), omni.BASE_TYPE_X86)


if __name__ == "__main__":
    unittest.main()
