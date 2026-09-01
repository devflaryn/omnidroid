#!/usr/bin/env python3
"""Turning autoexec off has to actually stop the scripts running.

    python3 tests/test_autoexec_switches.py

Two switches, and the first one is the one with a trap in it. The scripts do
NOT live on this machine at run time -- they live in the exec server's channel
for the account, put there by the last launch that pushed. So "disabled" can
never mean "skip the push": that leaves the previous bundle live and the
instance runs it anyway.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import autoexec  # noqa: E402


class OneScriptSwitchedOff(unittest.TestCase):
    """A disabled script keeps its contents AND its place in the sequence."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.dir = self.root / "autoexec"
        self.dir.mkdir()

    def _write(self, name, body="print('x')"):
        (self.dir / name).write_text(body, encoding="utf-8")

    def test_a_disabled_script_is_not_read(self):
        self._write("10-a.lua")
        self._write("20-b.lua.disabled")
        self.assertEqual([s["name"] for s in autoexec.read_scripts(self.root)],
                         ["10-a.lua"])

    def test_it_is_not_deleted(self):
        """'Off' has to be reversible without the user losing their script."""
        self._write("20-b.lua.disabled", "-- still here")
        self.assertEqual((self.dir / "20-b.lua.disabled").read_text(),
                         "-- still here")

    def test_run_order_survives_a_round_trip(self):
        """WHY A SUFFIX AND NOT A PREFIX. Filename order IS run order here, so
        a marker that changes the START of the name reorders the sequence the
        moment a script is switched back on."""
        self._write("10-a.lua")
        self._write("20-b.lua")
        self._write("30-c.lua")
        before = [s["name"] for s in autoexec.read_scripts(self.root)]
        (self.dir / "20-b.lua").rename(self.dir / "20-b.lua.disabled")
        self.assertEqual([s["name"] for s in autoexec.read_scripts(self.root)],
                         ["10-a.lua", "30-c.lua"])
        (self.dir / "20-b.lua.disabled").rename(self.dir / "20-b.lua")
        self.assertEqual([s["name"] for s in autoexec.read_scripts(self.root)],
                         before)

    def test_the_name_helpers_round_trip(self):
        self.assertEqual(autoexec.disabled_name("a.lua"), "a.lua.disabled")
        self.assertEqual(autoexec.enabled_name("a.lua.disabled"), "a.lua")
        # Idempotent both ways: the UI calls these on whatever it has.
        self.assertEqual(autoexec.disabled_name("a.lua.disabled"),
                         "a.lua.disabled")
        self.assertEqual(autoexec.enabled_name("a.lua"), "a.lua")
        self.assertTrue(autoexec.is_disabled("a.lua.disabled"))
        self.assertFalse(autoexec.is_disabled("a.lua"))


class TheMasterSwitchClearsTheChannel(unittest.TestCase):
    """⚠ THE WHOLE POINT. `OMNI_NO_AUTOEXEC` used to `return None` before the
    push, which left the LAST launch's bundle live in the server-side channel
    -- so the switch looked like it worked and the scripts ran anyway."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "autoexec").mkdir()
        (self.root / "autoexec" / "10-a.lua").write_text("print('a')",
                                                         encoding="utf-8")
        self.cfg = {"qemu": {"download_url": "http://example.invalid/x"}}

    def _push(self, env):
        """Run push_autoexec with the network stubbed; return the JSON sent."""
        sent = {}

        class _Resp:
            def read(self):
                return b'{"count": 0}'

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        def fake_urlopen(req, timeout=None):
            sent["payload"] = req.data
            return _Resp()

        with mock.patch.dict(os.environ, env, clear=False), \
             mock.patch.object(autoexec.urllib.request, "urlopen", fake_urlopen):
            autoexec.push_autoexec("acct", self.cfg, self.root, "t")
        return sent

    def test_disabled_still_posts_an_empty_bundle(self):
        sent = self._push({"OMNI_NO_AUTOEXEC": "1"})
        self.assertIn("payload", sent,
                      "disabled skipped the push, so the previous launch's "
                      "scripts are still live in the channel")
        self.assertIn(b'"scripts": []', sent["payload"])

    def test_enabled_posts_the_scripts(self):
        sent = self._push({"OMNI_NO_AUTOEXEC": ""})
        self.assertIn(b"10-a.lua", sent["payload"])

    def test_an_empty_folder_also_clears(self):
        """The behaviour the kill switch now matches, and always had."""
        (self.root / "autoexec" / "10-a.lua").unlink()
        sent = self._push({"OMNI_NO_AUTOEXEC": ""})
        self.assertIn(b'"scripts": []', sent["payload"])


if __name__ == "__main__":
    unittest.main()
