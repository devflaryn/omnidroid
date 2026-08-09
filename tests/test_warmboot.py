#!/usr/bin/env python3
"""Bake/restore orchestration and the post-restore clock resync.

    python3 -m pytest tests/test_warmboot.py -q

The QMP layer is faked: what matters here is the ORDER of operations and that
every failure degrades to "no entry / cold boot" rather than raising into a
launch.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import warmboot, warmcache  # noqa: E402


class FakeSession:
    """Records the command order and replays a scripted migration status.

    Also models the one side effect of a real `migrate`/`migrate-incoming`
    that this code actually depends on: QEMU writes the machine state to the
    `file:` URI it's given. `write_state=False` models a migrate that reports
    `completed` over QMP but never produced a usable state file (e.g. it
    crashed mid-stream) -- the exact case a naive implementation could
    silently publish as a good cache entry.
    """

    def __init__(self, port, migrate_status="completed", fail_on=None,
                 connect_timeout=60.0, timeout=15.0, write_state=True):
        self.calls = []
        self._status = migrate_status
        self._fail_on = fail_on or set()
        self._write_state = write_state

    def cmd(self, execute, arguments=None):
        self.calls.append(execute)
        if execute in self._fail_on:
            return {"error": {"desc": "nope"}}
        if execute in ("migrate", "migrate-incoming") and self._write_state:
            uri = (arguments or {}).get("uri", "")
            if uri.startswith("file:"):
                Path(uri[len("file:"):]).write_bytes(
                    b"fake-qemu-machine-state\n" * 64)
        return {"return": {}}

    def set_migration_caps(self, channels=4):
        self.calls.append("migrate-set-capabilities")

    def wait_migrate(self, timeout=600.0, sleep=0.25):
        self.calls.append("wait_migrate")
        return self._status

    def close(self):
        self.calls.append("close")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


class ClockResync(unittest.TestCase):
    """A restored guest wakes with the clock frozen at bake time; measured
    skew equals the wall time since the bake. -rtc base=utc,clock=host does
    NOT fix it. A wrong clock breaks TLS and cookie acceptance, which looks
    exactly like "auto-login is broken"."""

    def test_it_sets_the_guest_clock_to_host_time(self):
        sent = []

        class R:
            def __init__(self, out):
                self.stdout = out

        def fake_adb(acct, *args, **kw):
            sent.append(args)
            if args[:2] == ("shell", "date"):
                return R("1000\n" if len(sent) == 1 else "2000\n")
            return R("")

        skew = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=fake_adb, now_fn=lambda: 2000)

        self.assertEqual(skew, 1000)
        joined = [" ".join(a) for a in sent]
        self.assertTrue(any("date -s @2000" in j for j in joined), joined)

    def test_unreadable_guest_clock_returns_none_instead_of_raising(self):
        class R:
            stdout = "not-a-number"

        self.assertIsNone(warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=lambda *a, **k: R(),
            now_fn=lambda: 1))

    def test_adb_timeout_reading_guest_clock_returns_none_instead_of_raising(self):
        # The real adb() is subprocess.run(..., timeout=...), which raises
        # subprocess.TimeoutExpired -- NOT an OSError -- when a still-booting
        # guest never answers. That is the single most likely real-world
        # cause of an unreadable clock, so it must degrade like every other
        # failure here rather than propagate into the boot path.
        def timing_out(*a, **k):
            raise subprocess.TimeoutExpired(cmd="adb shell date +%s", timeout=20)

        self.assertIsNone(warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=timing_out, now_fn=lambda: 1))


class Restore(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.images, ignore_errors=True)
        self.entry = warmcache.entry_path(self.images, "k")
        self.entry.mkdir(parents=True, exist_ok=True)
        for n in warmcache.REQUIRED_FILES:
            (self.entry / n).write_bytes(b"x")

    def test_handshake_order_is_caps_then_incoming_then_cont(self):
        # Caps BEFORE migrate-incoming, or the load is rejected outright.
        sess = FakeSession(0)
        ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                   "lbl", session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        order = [c for c in sess.calls
                 if c in ("migrate-set-capabilities", "migrate-incoming",
                          "wait_migrate", "cont")]
        self.assertEqual(order, ["migrate-set-capabilities", "migrate-incoming",
                                 "wait_migrate", "cont"])

    def test_failed_migration_reports_false_and_never_conts(self):
        sess = FakeSession(0, migrate_status="failed")
        ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                   "lbl", session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertNotIn("cont", sess.calls)

    def test_qmp_that_never_answers_is_false_not_an_exception(self):
        def boom(*a, **k):
            raise OSError("no QMP")

        self.assertFalse(warmboot.restore_into(
            {"name": "t", "qmp_port": 1}, self.entry, "lbl",
            session_factory=boom))


class Bake(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.rd = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.images, ignore_errors=True)
        self.addCleanup(shutil.rmtree, self.rd, ignore_errors=True)
        for n in ("bake_system.qcow2", "bake_data.qcow2", "efivars.fd"):
            (self.rd / n).write_bytes(b"x")

    def test_successful_bake_publishes_a_findable_entry(self):
        sess = FakeSession(0)
        ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                 "k", {"qemu_version": "11.0.2"}, self.rd,
                                 "lbl", session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        self.assertIsNotNone(warmcache.lookup(self.images, "k", "11.0.2"))
        # The single most safety-critical property of a bake: a resumed
        # guest would keep writing to the very overlays the state file
        # describes, silently diverging them from what was just captured.
        self.assertNotIn("cont", sess.calls)

    def test_it_stops_the_vm_before_migrating(self):
        # Migrating a running guest would capture a torn machine.
        sess = FakeSession(0)
        warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images, "k",
                            {"qemu_version": "11.0.2"}, self.rd, "lbl",
                            session_factory=lambda *a, **k: sess)
        self.assertLess(sess.calls.index("stop"), sess.calls.index("migrate"))

    def test_failed_bake_leaves_no_entry_behind(self):
        sess = FakeSession(0, migrate_status="failed")
        ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                 "k", {"qemu_version": "11.0.2"}, self.rd,
                                 "lbl", session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertIsNone(warmcache.lookup(self.images, "k", "11.0.2"))
        self.assertFalse(any(p.name.startswith(".bake-")
                             for p in warmcache.warm_root(self.images).iterdir()))

    def test_bake_never_raises_into_the_caller(self):
        def boom(*a, **k):
            raise OSError("no QMP")

        self.assertFalse(warmboot.bake_entry(
            {"name": "t", "qmp_port": 1}, self.images, "k",
            {"qemu_version": "11.0.2"}, self.rd, "lbl", session_factory=boom))

    def test_migrate_completed_without_usable_state_reports_failure(self):
        # QMP can report "completed" while the state file itself is missing
        # or truncated (e.g. QEMU died mid-stream). lookup()'s REQUIRED_FILES
        # check exists precisely to catch this -- a bake must never route
        # around it (e.g. by pre-writing a placeholder) to make that check
        # pass; a truncated/missing state must stay a miss AND bake_entry
        # must itself report False, not just leave an entry lookup() will
        # later reject. Trusting query-migrate's status alone here would
        # mean every future launch pays the stop+migrate cost, logs a false
        # success, and still cold-boots -- forever, silently.
        sess = FakeSession(0, write_state=False)
        ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images, "k",
                                 {"qemu_version": "11.0.2"}, self.rd, "lbl",
                                 session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertIsNone(warmcache.lookup(self.images, "k", "11.0.2"))


if __name__ == "__main__":
    unittest.main()
