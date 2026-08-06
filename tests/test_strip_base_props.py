#!/usr/bin/env python3
"""Integration test for the build.prop surgery `omni strip-base` performs.

    python3 tests/test_strip_base_props.py

Builds a REAL (tiny) ext4 filesystem containing a build.prop with an SELinux
label, runs _bake_lean_props against it, and reads the result back out. That
exercises the whole debugfs sequence — dump, ea_get, rm, write, sif, ea_set,
readback — against a real filesystem rather than a mock.

Worth doing at this cost because the production target is a base image every
account's COW overlay is backed by: a half-written or unlabelled build.prop
there is not one broken instance, it is every instance. The full command adds
only a qcow2 round-trip and a partition lookup around this core.

Skipped when e2fsprogs is unavailable (macOS: brew install e2fsprogs).
"""
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import lean  # noqa: E402

# The profile is gated off by default (known to break boot); the
# surgery itself is still worth testing, so opt in explicitly here.
EXPERIMENTAL = lean.baked_props(include_unverified=True)

DBG = omni._debugfs_bin()
MKFS = omni._find_mke2fs()

ORIGINAL = (
    "# begin build properties\n"
    "ro.build.id=UQ1A.240205.004\n"
    "ro.config.low_ram=false\n"
    "ro.product.model=QEMU Virtual Machine\n"
    "dalvik.vm.heapsize=512m\n"
)
LABEL = "u:object_r:system_file:s0"


@unittest.skipIf(not DBG or not MKFS, "e2fsprogs (debugfs/mke2fs) not found")
class BakeLeanProps(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-striptest-"))
        staging = self.tmp / "root"
        (staging / "system").mkdir(parents=True)
        (staging / "system" / "build.prop").write_text(ORIGINAL)
        self.img = self.tmp / "system.img"
        subprocess.run(
            [MKFS, "-q", "-t", "ext4", "-d", str(staging), "-F",
             str(self.img), "16m"],
            check=True, capture_output=True, timeout=120)
        # Give build.prop the SELinux label the real image carries; the bake
        # refuses to write an unlabelled replacement, so without this we would
        # only ever exercise the refusal path.
        val = self.tmp / "label.val"
        val.write_bytes(LABEL.encode() + b"\x00")
        subprocess.run(
            [DBG, "-w", "-R",
             f"ea_set -f {val} /system/build.prop security.selinux",
             str(self.img)], check=True, capture_output=True, timeout=60)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _readback(self):
        out = self.tmp / "out.prop"
        subprocess.run([DBG, "-R", f"dump /system/build.prop {out}",
                        str(self.img)], capture_output=True, timeout=60)
        return out.read_text(errors="replace")

    def test_bake_succeeds_and_sets_every_property(self):
        err, path, n = omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        self.assertIsNone(err, err)
        self.assertEqual(path, "/system/build.prop")
        self.assertEqual(n, len(lean.baked_props(include_unverified=True)))
        text = self._readback()
        for k, v in lean.baked_props(include_unverified=True).items():
            self.assertIn(f"{k}={v}", text)

    def test_conflicting_readonly_property_is_replaced_not_appended(self):
        """The image ships ro.config.low_ram=false. init keeps the FIRST
        definition of a ro.* property, so leaving it in place would make the
        whole exercise a no-op that still reports success."""
        omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        text = self._readback()
        self.assertNotIn("ro.config.low_ram=false", text)
        self.assertEqual(text.count("ro.config.low_ram="), 1)
        self.assertIn("ro.config.low_ram=true", text)
        # The dalvik heap size is overridden the same way.
        self.assertNotIn("dalvik.vm.heapsize=512m", text)

    def test_unrelated_properties_survive(self):
        omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        text = self._readback()
        self.assertIn("ro.build.id=UQ1A.240205.004", text)
        self.assertIn("ro.product.model=QEMU Virtual Machine", text)
        self.assertIn("# begin build properties", text)

    def test_selinux_label_is_preserved(self):
        """An unlabelled build.prop is denied to the processes that read it,
        so the guest boots with none of these properties."""
        omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        r = subprocess.run([DBG, "-R", "ea_get /system/build.prop "
                            "security.selinux", str(self.img)],
                           capture_output=True, text=True, timeout=60)
        self.assertIn("system_file", r.stdout)

    def test_mode_and_ownership_are_restored(self):
        omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        r = subprocess.run([DBG, "-R", "stat /system/build.prop",
                            str(self.img)], capture_output=True, text=True,
                           timeout=60)
        # debugfs `stat` prints the permission bits alone ("Mode:  0644"),
        # not the full 0100644 that `sif ... mode` takes.
        self.assertRegex(r.stdout, r"Mode:\s+0644")
        self.assertRegex(r.stdout, r"User:\s+0")
        self.assertRegex(r.stdout, r"Group:\s+0")

    def test_is_idempotent(self):
        omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        first = self._readback()
        err, _, _ = omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        self.assertIsNone(err, err)
        self.assertEqual(self._readback(), first)

    def test_refuses_a_filesystem_with_no_build_prop(self):
        empty = self.tmp / "empty.img"
        subprocess.run([MKFS, "-q", "-t", "ext4", "-F", str(empty), "8m"],
                       check=True, capture_output=True, timeout=120)
        err, path, n = omni._bake_lean_props(str(empty), 0, "t",
                                     props=EXPERIMENTAL)
        self.assertIsNotNone(err)
        self.assertIn("no build.prop", err)
        self.assertIsNone(path)

    def test_refuses_when_the_selinux_label_is_missing(self):
        """Better to emit nothing than an image whose build.prop is unreadable
        to the processes that need it."""
        subprocess.run([DBG, "-w", "-R",
                        "ea_rm /system/build.prop security.selinux",
                        str(self.img)], capture_output=True, timeout=60)
        err, _, _ = omni._bake_lean_props(str(self.img), 0, "t", props=EXPERIMENTAL)
        self.assertIsNotNone(err)
        self.assertIn("SELinux label", err)
        # And the file is left exactly as it was.
        self.assertEqual(self._readback(), ORIGINAL)


if __name__ == "__main__":
    unittest.main()
