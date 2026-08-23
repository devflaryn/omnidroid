r"""A launch in flight owns its runtime directory. The sweep must not take it.

REPORTED FROM A USER'S FRESH INSTALL, 2026-08-22. A launch failed and the app
said:

    The virtual machine stopped before Android finished starting ...
    qemu.log could not be read ([Errno 2] No such file or directory:
    '...\OmniExec\runtime\<name>\qemu.log')

The log was not unwritten. It was DELETED, by us, while the failure was being
reported. `cmd_list` calls `reconcile_runtime()`, and the accounts panel polls
`list` every 4 seconds (frontend/src/engine.jsx) -- so within four seconds of
QEMU dying mid-boot, a sweep sees a record whose pid is dead and whose ports
are silent, decides the instance is abandoned, and wipes `runtime/<name>/`.

That costs two things, and the second is worse than the first:

  * `qemu.log` -- the ONLY place the reason for a QEMU death exists, since the
    process is detached and nothing else ever sees its stderr. Losing it turns
    a diagnosable failure into "could not be read".

  * `run.json` -- which `_boot_used_gpu` reads to decide whether the boot was
    rendering on the host GPU. With the record gone it answers False,
    `_display_is_implicated` answers False, and the GPU -> software fallback
    added the same day NEVER FIRES. The retry that would have rescued the boot
    is skipped precisely because the user was looking at the accounts tab.

The launcher's own pid is what distinguishes the two cases: while it is alive
the launch is still running and the directory is in use, whatever its QEMU is
doing. That is the same self-healing trick `_reserve_ports` already uses for
the window before spawn -- this extends it across the boot.
"""
import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from omnidroid import runtime  # noqa: E402


def _record(root, name, **extra):
    d = root / "runtime" / name
    d.mkdir(parents=True, exist_ok=True)
    rec = {"pid": 999999, "identity": f"omni-{name}",
           "qmp_port": 1, "adb_port": 2}
    rec.update(extra)
    (d / "run.json").write_text(json.dumps(rec))
    (d / "qemu.log").write_text("qemu-system-x86_64: the reason it died\n")
    return d


class SweepDuringALaunch(unittest.TestCase):
    """The sweep runs on a timer the user cannot see. It must not race a boot."""

    def setUp(self):
        import tempfile
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        patches = [
            mock.patch.object(runtime.config, "data_dir", lambda: self.root),
            mock.patch.object(runtime, "instance_live", lambda rec: False),
            mock.patch.object(runtime, "_port_answers",
                              lambda port, timeout=0.25: False),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        self.addCleanup(self._tmp.cleanup)

    def test_a_dead_qemu_whose_launcher_is_still_running_is_left_alone(self):
        """THE REPORTED CASE. QEMU is gone and the ports are silent -- which is
        exactly what a boot failure looks like -- but the launch that spawned
        it is still on its feet, reading the log to say why."""
        d = _record(self.root, "acc0", launcher_pid=os.getpid())

        result = runtime.reconcile_runtime()

        self.assertEqual(result["gc"], [])
        self.assertTrue((d / "qemu.log").exists(),
                        "the only record of why QEMU died must survive")
        self.assertTrue((d / "run.json").exists(),
                        "and so must the record the GPU fallback reads")

    def test_a_dead_qemu_whose_launcher_is_gone_is_still_collected(self):
        """The behaviour this must not break: a genuinely abandoned instance
        is still swept, which is the whole reason reconcile exists."""
        d = _record(self.root, "acc1", launcher_pid=999998)

        result = runtime.reconcile_runtime()

        self.assertIn("acc1", result["gc"])
        self.assertFalse(d.exists())

    def test_a_record_from_before_this_fix_is_still_collected(self):
        """No launcher_pid at all -- every run.json written by an older build.
        Absent must mean collectable, or upgrading strands every leftover."""
        d = _record(self.root, "acc2")

        result = runtime.reconcile_runtime()

        self.assertIn("acc2", result["gc"])
        self.assertFalse(d.exists())

    def test_the_launcher_that_wrote_it_does_not_protect_it_forever(self):
        """A reservation from a launcher that has exited is collectable too --
        the pid is the lease, and a dead lease-holder holds nothing."""
        d = _record(self.root, "acc3", launcher_pid=999997, reserving=False)

        runtime.reconcile_runtime()

        self.assertFalse(d.exists())


class TheLauncherIsRecorded(unittest.TestCase):
    """The guard is worth nothing if spawn_qemu does not write the pid."""

    def test_spawn_records_the_launching_process(self):
        source = (Path(__file__).resolve().parent.parent / "omnidroid"
                  / "qemu_proc.py").read_text(encoding="utf-8", errors="ignore")
        self.assertIn('"launcher_pid": os.getpid()', source,
                      "spawn_qemu must stamp the run record with the pid of "
                      "the process waiting on this boot")


if __name__ == "__main__":
    unittest.main()
