"""The host-pointer policy: one pointer on screen, ever.

Pure parts only. The QMP command itself lives in qemu-patches/0010 and is
exercised by booting; what a unit test can pin is the decision, the log
reading, and the argv/flags the engine emits.
"""
import unittest
from pathlib import Path

from omnidroid import hostcursor, qemu_proc
from omnidroid.hostcursor import (host_cursor_visible, in_place_from_log,
                                  parse_probe, probe_script)

JOINED = (r"Connection accepted", r"clientReplicator",
          r"DataModel::doDataModelSetup", r"Received CLIENT_ID")


class Decision(unittest.TestCase):
    """Fails towards VISIBLE: the only state that blanks the host pointer is
    a client known to be in a place with its process not known gone."""

    def test_in_place_and_running_hides(self):
        self.assertFalse(host_cursor_visible(True, True))
        self.assertFalse(host_cursor_visible(True, None))

    def test_everything_else_shows(self):
        self.assertTrue(host_cursor_visible(False, True))
        self.assertTrue(host_cursor_visible(None, True))
        self.assertTrue(host_cursor_visible(None, None))
        self.assertTrue(host_cursor_visible(True, False))


class LogReading(unittest.TestCase):
    def test_no_markers_is_unknown(self):
        self.assertIsNone(in_place_from_log("", JOINED))
        self.assertIsNone(in_place_from_log("[FLog::Output] hello", JOINED))

    def test_join_then_nothing_is_in_place(self):
        log = "x\n[FLog::Network] Connection accepted from 1.2.3.4\ny\n"
        self.assertTrue(in_place_from_log(log, JOINED))

    def test_join_then_leave_is_out(self):
        log = ("[FLog::Network] Connection accepted from 1.2.3.4\n"
               "[FLog::Network] Client:Disconnect\n")
        self.assertFalse(in_place_from_log(log, JOINED))

    def test_leave_then_rejoin_is_in_place_again(self):
        log = ("[FLog::Network] Connection accepted from 1.2.3.4\n"
               "[FLog::Network] Sending disconnect with reason: 1\n"
               "[FLog::Network] Connection accepted from 5.6.7.8\n")
        self.assertTrue(in_place_from_log(log, JOINED))

    def test_position_not_count_decides(self):
        """Three old joins do not outvote one recent leave."""
        log = ("Connection accepted\nConnection accepted\n"
               "Connection accepted\nClient:Disconnect\n")
        self.assertFalse(in_place_from_log(log, JOINED))


class Probe(unittest.TestCase):
    def test_script_answers_both_questions_in_one_trip(self):
        s = probe_script("com.roblox.client",
                         "/data/data/com.roblox.client/files/appData/logs")
        self.assertIn("pidof com.roblox.client", s)
        self.assertIn("__OMNI_SEP__", s)
        self.assertIn("tail -c", s)
        self.assertIn("ls -t", s)

    def test_script_quotes_the_package(self):
        s = probe_script("bad name; rm -rf /", "/tmp")
        self.assertNotIn("pidof bad name;", s)

    def test_parse_running_and_tail(self):
        running, seen, tail = parse_probe(
            "1234\n__OMNI_SEP__\nplayer.log 4096\nlog line\n")
        self.assertTrue(running)
        self.assertEqual(seen, ("player.log", 4096))
        self.assertEqual(tail, "log line")

    def test_parse_gone(self):
        running, seen, tail = parse_probe(
            "\n__OMNI_SEP__\nplayer.log 10\nlog line\n")
        self.assertFalse(running)

    def test_parse_no_log_yet(self):
        running, seen, tail = parse_probe("1234\n__OMNI_SEP__\n")
        self.assertTrue(running)
        self.assertIsNone(seen)
        self.assertEqual(tail, "")

    def test_parse_no_answer_is_unknown(self):
        self.assertEqual(parse_probe(""), (None, None, ""))
        self.assertEqual(parse_probe("garbage"), (None, None, ""))

    def test_script_reads_only_what_is_new(self):
        """Markers scroll out of any fixed tail within minutes of joining;
        the probe therefore reads from where it left off, and a rotated or
        shorter file falls back to its tail."""
        s = probe_script("com.roblox.client", "/logs", seen=("a.log", 5000))
        self.assertIn("tail -c +5001", s)
        self.assertIn('"$L" = a.log', s)
        self.assertIn("tail -c 48000", s)      # the fallback branch


class EngineSide(unittest.TestCase):
    def test_gtk_window_shows_the_host_pointer(self):
        """The guest no longer paints one (pointer overlay), so the host's
        must be on -- a stock suboption, on every build."""
        self.assertIn("show-cursor=on", qemu_proc.window_flags("gtk"))

    def test_support_is_read_from_the_capability_token(self):
        real = qemu_proc._omni_caps_of
        try:
            qemu_proc._omni_caps_of = lambda b: ("omni-window",
                                                 "omni-host-cursor")
            self.assertTrue(qemu_proc.qemu_supports_host_cursor())
            qemu_proc._omni_caps_of = lambda b: ("omni-window",)
            self.assertFalse(qemu_proc.qemu_supports_host_cursor())
        finally:
            qemu_proc._omni_caps_of = real

    def test_set_host_cursor_reports_acknowledgement_only(self):
        calls = []

        def fake_qmp(acct, execute, arguments=None, timeout=6):
            calls.append((execute, arguments))
            return {"return": {}} if arguments["visible"] else None
        real = qemu_proc.qmp
        try:
            qemu_proc.qmp = fake_qmp
            self.assertTrue(qemu_proc.set_host_cursor({"qmp_port": 1}, True))
            self.assertFalse(qemu_proc.set_host_cursor({"qmp_port": 1}, False))
        finally:
            qemu_proc.qmp = real
        self.assertEqual(calls[0], ("omni-host-cursor", {"visible": True}))
        self.assertEqual(calls[1], ("omni-host-cursor", {"visible": False}))

    def test_patch_series_carries_0010(self):
        series = (Path(__file__).resolve().parent.parent / "qemu-patches"
                  / "SERIES").read_text()
        self.assertIn("0010-omni-host-cursor.patch", series)
        self.assertGreater(hostcursor.POLL_SECS, 0)


if __name__ == "__main__":
    unittest.main()
