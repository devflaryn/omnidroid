"""The scratch: where a diskless guest's writes actually live.

Every ephemeral boot runs `snapshot=on`, so QEMU keeps the guest's writes in a
temporary overlay and drops it at exit. Left to itself that file goes to the
libc temp directory -- `%TEMP%` on Windows -- where nothing looks at it.

MEASURED 2026-08-15 on the Windows dev box, PS99, one farming instance: the
overlay reached 1.3 GB, and three sessions' worth had leaked into `%TEMP%`
(3.7 GB, the oldest two days old) because a QEMU that dies rather than exits
never unlinks its own file. With the volume down to 4 GB free, instances began
dying three minutes in with a ZERO-BYTE qemu.log -- QEMU cannot write the
reason down when writing it needs the disk that just ran out.

So these tests pin the three things that failure needed: the overlay goes
somewhere we own, leaked ones are reaped, and a launch that cannot fit says so
instead of finding out later.
"""

import os
import unittest
from pathlib import Path
from unittest import mock

from omnidroid import qemu_proc as qp


class ScratchLocation(unittest.TestCase):
    def test_default_is_beside_runtime_not_inside_it(self):
        """reconcile_runtime() reports every run.json-less directory under
        runtime/ as an orphaned instance, so a scratch dir in there would be
        reported as one on every single sweep."""
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("OMNI_SCRATCH_DIR", None)
            d = qp.scratch_dir()
        self.assertIsNotNone(d)
        self.assertEqual(d.name, qp.SCRATCH_DIRNAME)
        self.assertNotEqual(d.parent.name, "runtime")

    def test_env_beats_config(self):
        with mock.patch.dict(os.environ,
                             {"OMNI_SCRATCH_DIR": str(Path.cwd() / "_envscr")}):
            d = qp.scratch_dir({"qemu": {"scratch_dir": "/cfg/scratch"}})
        self.assertEqual(d.name, "_envscr")
        d.rmdir()

    def test_config_used_when_env_absent(self):
        target = Path.cwd() / "_cfgscr"
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("OMNI_SCRATCH_DIR", None)
            d = qp.scratch_dir({"qemu": {"scratch_dir": str(target)}})
        self.assertEqual(d, target)
        d.rmdir()

    def test_unwritable_location_is_none_not_an_exception(self):
        """This runs on the boot path. A scratch we cannot create means we
        stop redirecting QEMU, never that the launch fails."""
        with mock.patch.object(Path, "mkdir", side_effect=OSError("denied")):
            self.assertIsNone(qp.scratch_dir({"qemu": {"scratch_dir": "/nope"}}))


class ScratchEnv(unittest.TestCase):
    def test_sets_all_three_names(self):
        """Windows' GetTempPath reads TMP then TEMP; glib/glibc read TMPDIR.
        Setting only the one that matters on the platform you happen to be
        testing is how this silently reverts on the other two."""
        target = Path.cwd() / "_envall"
        with mock.patch.dict(os.environ, {"OMNI_SCRATCH_DIR": str(target)}):
            env = qp.scratch_env()
        for name in ("TMP", "TEMP", "TMPDIR"):
            self.assertEqual(env[name], str(target), name)
        target.rmdir()

    def test_inherits_the_rest_of_the_environment(self):
        """QEMU needs PATH and the user's profile; this is an override, not a
        replacement."""
        with mock.patch.dict(os.environ, {"OMNI_SCRATCH_TEST_MARKER": "kept"}):
            env = qp.scratch_env()
        self.assertEqual(env.get("OMNI_SCRATCH_TEST_MARKER"), "kept")

    def test_no_scratch_leaves_the_environment_alone(self):
        with mock.patch.object(qp, "scratch_dir", return_value=None):
            env = qp.scratch_env()
        self.assertEqual(env.get("TMP"), os.environ.get("TMP"))


class ScratchRoom(unittest.TestCase):
    def test_room_when_free_exceeds_instance_plus_floor(self):
        with mock.patch.object(qp, "scratch_free_mb", return_value=99_000):
            room, free, needed = qp.scratch_room()
        self.assertTrue(room)
        self.assertEqual(free, 99_000)
        self.assertEqual(needed,
                         qp.SCRATCH_PER_INSTANCE_MB + qp.SCRATCH_FLOOR_MB)

    def test_no_room_when_free_is_below_the_floor(self):
        with mock.patch.object(qp, "scratch_free_mb", return_value=1_000):
            room, free, _ = qp.scratch_room()
        self.assertFalse(room)
        self.assertEqual(free, 1_000)

    def test_unknown_free_space_means_yes(self):
        """A disk-usage call that fails on some future host is a reason to
        stop asking, not a reason to refuse to boot."""
        with mock.patch.object(qp, "scratch_free_mb", return_value=None):
            room, free, _ = qp.scratch_room()
        self.assertTrue(room)
        self.assertIsNone(free)

    def test_caller_can_name_a_bigger_instance(self):
        with mock.patch.object(qp, "scratch_free_mb", return_value=5_000):
            self.assertFalse(qp.scratch_room(want_mb=8_000)[0])
            self.assertTrue(qp.scratch_room(want_mb=1_000)[0])


class ScratchReaper(unittest.TestCase):
    def setUp(self):
        self.dir = Path.cwd() / "_reapscr"
        self.dir.mkdir(exist_ok=True)
        self.patch = mock.patch.object(qp, "scratch_dir", return_value=self.dir)
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def tearDown(self):
        for p in self.dir.glob("*"):
            p.unlink()
        self.dir.rmdir()

    def _overlay(self, name, mb=1):
        p = self.dir / name
        p.write_bytes(b"\0" * (mb * 1024 * 1024))
        return p

    def test_reaps_qemu_overlays_and_reports_megabytes(self):
        self._overlay("vl.ABC123", mb=2)
        self._overlay("vl.DEF456", mb=3)
        files, megabytes = qp.reap_scratch()
        self.assertEqual(files, 2)
        self.assertEqual(megabytes, 5)

    def test_leaves_anything_that_is_not_a_qemu_overlay(self):
        """The scratch is a plain directory a user may have pointed somewhere
        shared. Match QEMU's own `vl.XXXXXX` template, never `*`."""
        keep = self.dir / "important.qcow2"
        keep.write_bytes(b"x")
        self._overlay("vl.KEEPNOT")
        qp.reap_scratch()
        self.assertTrue(keep.exists())

    def test_a_locked_overlay_is_skipped_not_raised(self):
        """On Windows a running guest's open handle refuses the unlink, and
        that refusal is exactly what makes the reaper safe to run at any
        time -- including from the boot path, where raising is not allowed."""
        self._overlay("vl.LOCKED")
        with mock.patch.object(Path, "unlink",
                               side_effect=PermissionError("in use")):
            files, megabytes = qp.reap_scratch()
        self.assertEqual((files, megabytes), (0, 0))

    def test_missing_scratch_is_zero_not_an_exception(self):
        with mock.patch.object(qp, "scratch_dir", return_value=None):
            self.assertEqual(qp.reap_scratch(), (0, 0))

    @unittest.skipIf(qp.IS_WINDOWS, "POSIX-only: Windows unlink refuses instead")
    def test_posix_spares_an_overlay_a_live_guest_still_holds(self):
        """Unlinking an open file succeeds on POSIX and the guest loses every
        write it has made -- so there the owner has to be checked explicitly."""
        self._overlay("vl.INUSE")
        with mock.patch.object(qp, "_scratch_owner", return_value=4242):
            files, _ = qp.reap_scratch(live_pids=[4242])
        self.assertEqual(files, 0)


class SpawnUsesTheScratch(unittest.TestCase):
    def test_spawn_passes_the_scratch_environment_to_qemu(self):
        """The whole mechanism is one `env=` on one Popen. A refactor that
        drops it puts 1.3 GB per instance back in %TEMP% invisibly."""
        import inspect
        src = inspect.getsource(qp.spawn_qemu)
        self.assertIn("env=scratch_env(cfg)", src)


if __name__ == "__main__":
    unittest.main()
