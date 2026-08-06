#!/usr/bin/env python3
"""Does a REAL QEMU accept the device set we build? (x86 path)

    python3 tests/test_qemu_accepts_devices.py

Every other test in this suite checks the command line we *intend* to emit.
That is not the same as QEMU agreeing to build the machine, and the gap is
not hypothetical: this session shipped ~200 lines of fstab surgery whose unit
tests all passed while the code probed paths that did not exist on the real
image. Unit tests confirm assumptions; only the real binary confirms reality.

So this constructs the machine for real with `-S` (build everything, do not
run the CPU), then asks QMP whether it came up and whether the balloon device
is actually there. It is the cheapest possible check that:

  * every -device we emit exists for this machine type,
  * `virtio-balloon-pci,free-page-reporting=on` is valid on x86/q35 as well
    as on arm (the memory model depends on it on BOTH arches, and only arm
    had ever been booted),
  * the USB set and the flags survive together.

x86 specifically because it is the arch that cannot be booted on this
project's Apple Silicon dev machine without TCG, so it is the one most likely
to rot unnoticed.

Skipped when qemu-system-x86_64 or the x86 base files are absent.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402

QEMU = shutil.which("qemu-system-x86_64")
QEMU_IMG = shutil.which("qemu-img")

try:
    _CFG = omni.load_config()
    _IMAGES = Path(_CFG["images_dir"])
    _X86 = (_CFG.get("bases") or {}).get("x86") or {}
    _HAVE_BASE = bool(_X86) and all(
        (_IMAGES / _X86[k]).exists() for k in ("kernel", "initrd")
        if _X86.get(k))
except Exception:                                    # pragma: no cover
    _CFG, _X86, _HAVE_BASE = {}, {}, False

# Ports well clear of the allocator's range so a live fleet is never touched.
ADB, QMP_PORT, VNC = 16997, 17997, 18997


@unittest.skipIf(not QEMU or not QEMU_IMG, "qemu-system-x86_64 not installed")
@unittest.skipIf(not _HAVE_BASE, "x86 base kernel/initrd not present")
class QemuAcceptsX86Devices(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="omni-x86dev-"))
        for name in ("system.qcow2", "data.qcow2"):
            subprocess.run([QEMU_IMG, "create", "-f", "qcow2",
                            str(cls.tmp / name), "64M"],
                           check=True, capture_output=True, timeout=60)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def _construct(self, mode_name):
        """Build the machine with -S and return (status, balloon_ok)."""
        acct = {"name": "x86devtest", "adb_port": ADB, "qmp_port": QMP_PORT,
                "vnc_port": VNC, "base": "x86"}
        with mock.patch("omnidroid.engine.account_dir", return_value=self.tmp):
            cmd = qemu_proc.qemu_command(
                acct, _CFG, False,
                mode=omni.resolve_mode(_CFG, mode_name), accel="tcg")
        proc = subprocess.Popen(cmd + ["-S"], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE)
        try:
            status = balloon = None
            for _ in range(20):
                time.sleep(0.5)
                if proc.poll() is not None:
                    err = proc.stderr.read().decode(errors="replace")
                    self.fail(f"QEMU refused the {mode_name} device set "
                              f"(rc={proc.returncode}): {err.strip()[-400:]}")
                r = qemu_proc.qmp({"qmp_port": QMP_PORT}, "query-status")
                if r and "return" in r:
                    status = r["return"]
                    b = qemu_proc.qmp({"qmp_port": QMP_PORT}, "query-balloon")
                    balloon = bool(b and "return" in b)
                    break
            return status, balloon
        finally:
            qemu_proc.qmp({"qmp_port": QMP_PORT}, "quit")
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:        # pragma: no cover
                proc.kill()
                proc.wait(timeout=10)

    def test_farming_machine_is_constructible(self):
        status, balloon = self._construct("farming")
        self.assertIsNotNone(status, "QMP never answered")
        # -S means built-but-not-running.
        self.assertFalse(status.get("running"))
        self.assertTrue(balloon,
                        "virtio-balloon-pci is missing on x86 - the whole "
                        "memory model depends on it")

    def test_playable_machine_is_constructible(self):
        status, balloon = self._construct("playable")
        self.assertIsNotNone(status, "QMP never answered")
        self.assertTrue(balloon)


if __name__ == "__main__":
    unittest.main()
