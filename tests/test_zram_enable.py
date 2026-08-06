#!/usr/bin/env python3
"""Enabling zram on a NON-ROOTED production instance.

    python3 tests/test_zram_enable.py

zram is worth a third of the per-instance footprint (lz4 compressed 496 MB of
guest pages into 167 MB, 2.97x measured, dropping the safe balloon cap from
1536 to 1024 MB).

The base already ships the whole mechanism — read off a live instance before
any of this was written: /vendor/etc/fstab.virtio carries
`/dev/block/zram0 none swap defaults zramsize=50%`, and
/vendor/etc/init/zram.rc modprobes zram, sets lz4, and runs `swapon_all` on
`persist.sys.zram_enabled=1`. So enabling zram is ONE property, and baking it
reuses the build.prop surgery that test_strip_base_props.py already covers.

Verified end-to-end on the real base: setting that property made init run
swapon_all and SwapTotal went 0 -> 470980 kB. It is settable at runtime
(persist.*, not ro.*) but denied to uid shell by SELinux, which is why
production needs it baked.
"""
import os
import shutil as _sh
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import lean  # noqa: E402

QEMU_IMG = _sh.which("qemu-img")


class ZramEnableProp(unittest.TestCase):
    def test_is_the_lineageos_toggle(self):
        self.assertEqual(lean.ZRAM_ENABLE_PROP,
                         {"persist.sys.zram_enabled": "1"})

    def test_is_persist_not_ro(self):
        """ro.* is frozen by init once set; persist.* can be flipped at
        runtime, which is what lets the dev base enable zram without a
        rebuild."""
        key = next(iter(lean.ZRAM_ENABLE_PROP))
        self.assertTrue(key.startswith("persist."))
        self.assertFalse(key.startswith("ro."))

    def test_merges_into_build_prop_without_disturbing_anything(self):
        out = lean.merge_build_prop("ro.build.id=X\n", lean.ZRAM_ENABLE_PROP)
        self.assertIn("persist.sys.zram_enabled=1", out)
        self.assertIn("ro.build.id=X", out)

    def test_merge_replaces_a_disabled_value(self):
        """If the image ships it set to 0, appending would leave 0 winning
        for a ro.* key and is simply confusing for a persist one."""
        out = lean.merge_build_prop("persist.sys.zram_enabled=0\n",
                                    lean.ZRAM_ENABLE_PROP)
        self.assertNotIn("persist.sys.zram_enabled=0", out)
        self.assertEqual(out.count("persist.sys.zram_enabled"), 1)

    def test_is_not_part_of_the_gated_lean_profile(self):
        """The lean profile is gated off because it breaks boot. This
        property is unrelated to it and must not be dragged into that gate."""
        self.assertNotIn(next(iter(lean.ZRAM_ENABLE_PROP)),
                         lean.baked_props(include_unverified=True))


class ReclaimableBackups(unittest.TestCase):
    """On a full disk, "free some space" is not an answer; which files are
    redundant is. This ADVISES only - it must never propose a last copy."""

    def setUp(self):
        self.d = Path(tempfile.mkdtemp(prefix="omni-reclaim-"))

    def tearDown(self):
        import shutil
        shutil.rmtree(self.d, ignore_errors=True)

    def _mk(self, name, size=1024):
        (self.d / name).write_bytes(b"\0" * size)

    def test_lists_a_backup_whose_original_exists(self):
        self._mk("base.qcow2")
        self._mk("base.qcow2.bak", 2048)
        got = [p.name for p, _ in omni.reclaimable_backups(self.d)]
        self.assertEqual(got, ["base.qcow2.bak"])

    def test_never_lists_a_backup_whose_original_is_GONE(self):
        """That file is the only copy left - proposing it is data loss."""
        self._mk("orphan.qcow2.bak")
        self.assertEqual(omni.reclaimable_backups(self.d), [])

    def test_handles_the_safebak_suffix(self):
        self._mk("x.qcow2")
        self._mk("x.qcow2.safebak-20260720", 4096)
        got = [p.name for p, _ in omni.reclaimable_backups(self.d)]
        self.assertEqual(got, ["x.qcow2.safebak-20260720"])

    def test_never_lists_a_live_base_image(self):
        self._mk("base_arm_v2.qcow2")
        self._mk("base_arm_system.qcow2")
        self.assertEqual(omni.reclaimable_backups(self.d), [])

    def test_message_names_the_files_and_does_not_delete(self):
        self._mk("base.qcow2")
        self._mk("base.qcow2.bak", 4096)
        msg = omni._scratch_help(self.d, 1, 6 * 1024 ** 3)
        self.assertIn("base.qcow2.bak", msg)
        self.assertIn("will not touch them", msg)
        self.assertTrue((self.d / "base.qcow2.bak").exists())


@unittest.skipIf(not QEMU_IMG, "qemu-img not installed")
class ScratchSizing(unittest.TestCase):
    """The round trip needs the RAW export plus a thin overlay, not two full
    copies. Over-stating it is not harmless: it is the difference between
    telling someone to free one stale backup and telling them to free three,
    on a disk that is already full."""

    def setUp(self):
        self.d = Path(tempfile.mkdtemp(prefix="omni-scratch-"))

    def tearDown(self):
        import shutil
        shutil.rmtree(self.d, ignore_errors=True)

    def test_sized_from_the_images_actual_size(self):
        img = self.d / "b.qcow2"
        subprocess.run([QEMU_IMG, "create", "-f", "qcow2", str(img), "5G"],
                       check=True, capture_output=True, timeout=60)
        need = omni.scratch_needed(img)
        # An empty 5 GiB qcow2 allocates almost nothing, so the requirement
        # should be dominated by the margin - not by the virtual size.
        self.assertLess(need, 2 * 1024 ** 3)
        self.assertGreaterEqual(need, 1024 ** 3)

    def test_falls_back_conservatively_when_the_image_is_unreadable(self):
        """Guessing LOW would fail halfway through a multi-GiB conversion."""
        self.assertEqual(omni.scratch_needed(self.d / "nope.qcow2"),
                         6 * 1024 ** 3)

    def test_backing_args_match_this_qemu(self):
        """The flag was renamed (-B up to QEMU 10.0, -b + -F after), so it is
        probed from the help text rather than assumed."""
        args = omni._backing_args(Path("/x/base.qcow2"))
        self.assertIn("/x/base.qcow2", args)
        self.assertIn(args[0], ("-b", "-B"))
        if args[0] == "-b":
            self.assertIn("-F", args)


class ScratchDirEscapeHatch(unittest.TestCase):
    """A tool must never leave "delete your backups" as the ONLY way out.

    The round trip's peak cost is a temporary raw export, so it can live on
    any volume. On a full internal disk an external drive is a strictly
    better answer than reclaiming someone's rollback copies."""

    def setUp(self):
        self.d = Path(tempfile.mkdtemp(prefix="omni-scratchdir-"))

    def tearDown(self):
        import shutil
        shutil.rmtree(self.d, ignore_errors=True)

    def test_message_offers_relocating_the_scratch(self):
        msg = omni._scratch_help(self.d, 1, 6 * 1024 ** 3)
        self.assertIn("--scratch-dir", msg)

    def test_message_names_the_directory_that_is_short(self):
        msg = omni._scratch_help(self.d, 1, 6 * 1024 ** 3)
        self.assertIn(str(self.d), msg)

    def test_backups_are_looked_for_in_the_images_dir_not_the_scratch(self):
        """With scratch on another volume, the reclaimable images still live
        with the base images - look there, not where the temp file goes."""
        images = Path(tempfile.mkdtemp(prefix="omni-imgs-"))
        try:
            (images / "b.qcow2").write_bytes(b"\0" * 16)
            (images / "b.qcow2.bak").write_bytes(b"\0" * 32)
            msg = omni._scratch_help(self.d, 1, 6 * 1024 ** 3, images=images)
            self.assertIn("b.qcow2.bak", msg)
        finally:
            import shutil
            shutil.rmtree(images, ignore_errors=True)

    def test_rejects_a_scratch_dir_that_is_not_a_directory(self):
        import types
        with self.assertRaises(SystemExit):
            omni.cmd_enable_zram_base(types.SimpleNamespace(
                base=None, out=None, in_place=False, json=False,
                scratch_dir=str(self.d / "does-not-exist")))


if __name__ == "__main__":
    unittest.main()
