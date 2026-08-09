#!/usr/bin/env python3
"""A persistent QMP session, driven against a fake QMP server.

    python3 -m pytest tests/test_qmpsession.py -q

qemu_proc.qmp() opens one connection per command, which cannot express the
migration handshake: capabilities must be set and `migrate-incoming` issued on
the SAME session, or the load dies with
"Capability mapped-ram is off, but received capability is on".
"""
import json
import os
import socket
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid.qmpsession import QmpSession  # noqa: E402


class FakeQmp:
    """Minimal QMP server: greets, then answers each command from a script."""

    def __init__(self, replies):
        self.replies = list(replies)
        self.received = []
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.port = self.sock.getsockname()[1]
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self):
        conn, _ = self.sock.accept()
        f = conn.makefile("rw", encoding="utf-8", newline="\n")
        f.write(json.dumps({"QMP": {"version": {}}}) + "\n")
        f.flush()
        while True:
            line = f.readline()
            if not line:
                break
            self.received.append(json.loads(line))
            reply = self.replies.pop(0) if self.replies else {"return": {}}
            f.write(json.dumps(reply) + "\n")
            f.flush()
        conn.close()


class Session(unittest.TestCase):
    def test_capabilities_are_negotiated_once_on_connect(self):
        fake = FakeQmp([{"return": {}}])
        with QmpSession(fake.port) as s:
            s.cmd("query-status")
        self.assertEqual(fake.received[0]["execute"], "qmp_capabilities")

    def test_cmd_returns_the_parsed_reply(self):
        fake = FakeQmp([{"return": {}},
                        {"return": {"status": "paused", "running": False}}])
        with QmpSession(fake.port) as s:
            r = s.cmd("query-status")
        self.assertEqual(r["return"]["status"], "paused")

    def test_migration_caps_enable_mapped_ram_and_multifd_together(self):
        # Both are required: the source writes a mapped-ram stream and the
        # destination refuses it unless it agreed to the same capability.
        fake = FakeQmp([{"return": {}}, {"return": {}}, {"return": {}}])
        with QmpSession(fake.port) as s:
            s.set_migration_caps(channels=4)
        caps = [m for m in fake.received
                if m["execute"] == "migrate-set-capabilities"][0]
        enabled = {c["capability"]: c["state"]
                   for c in caps["arguments"]["capabilities"]}
        self.assertEqual(enabled, {"mapped-ram": True, "multifd": True})
        params = [m for m in fake.received
                  if m["execute"] == "migrate-set-parameters"][0]
        self.assertEqual(params["arguments"]["multifd-channels"], 4)

    def test_wait_migrate_polls_until_a_terminal_status(self):
        fake = FakeQmp([{"return": {}},
                        {"return": {"status": "active"}},
                        {"return": {"status": "active"}},
                        {"return": {"status": "completed"}}])
        with QmpSession(fake.port) as s:
            self.assertEqual(s.wait_migrate(timeout=5, sleep=0), "completed")

    def test_wait_migrate_gives_up_and_reports_rather_than_hanging(self):
        fake = FakeQmp([{"return": {}}] + [{"return": {"status": "active"}}] * 50)
        with QmpSession(fake.port) as s:
            self.assertEqual(s.wait_migrate(timeout=0, sleep=0), "timeout")

    def test_connect_failure_raises_a_clear_error(self):
        # Port 1 is never a QMP server; the caller must be able to catch this
        # and fall back to a cold boot.
        with self.assertRaises(OSError):
            QmpSession(1, connect_timeout=0.5)


if __name__ == "__main__":
    unittest.main()
