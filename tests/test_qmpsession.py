#!/usr/bin/env python3
"""A persistent QMP session, driven against a fake QMP server.

    python3 -m pytest tests/test_qmpsession.py -q

qemu_proc.qmp() opens one connection per command, which cannot express the
migration handshake: capabilities must be set and `migrate-incoming` issued on
the SAME session, or the load dies with
"Capability mapped-ram is off, but received capability is on".
"""
import contextlib
import json
import os
import socket
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid.qmpsession import QmpSession  # noqa: E402


class FakeQmp:
    """Minimal QMP server: greets, then answers each command from a script.

    Each scripted item is normally a single reply dict. An item may instead
    be a LIST of messages, all written for that one received command, in
    order -- this is how a test simulates QMP interleaving an unsolicited
    async *event* before the real reply.
    """

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
            messages = reply if isinstance(reply, list) else [reply]
            for m in messages:
                f.write(json.dumps(m) + "\n")
                f.flush()
        conn.close()


@contextlib.contextmanager
def _spy_created_sockets():
    """Capture every socket QmpSession opens via socket.create_connection,
    so a test can assert it was closed (not leaked) after a failed handshake.
    A closed socket's fileno() reads back as -1."""
    created = []
    orig = socket.create_connection

    def spy(*a, **kw):
        sock = orig(*a, **kw)
        created.append(sock)
        return sock

    socket.create_connection = spy
    try:
        yield created
    finally:
        socket.create_connection = orig


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

    def test_wait_migrate_treats_an_error_reply_as_terminal_not_a_hang(self):
        # A dead QEMU on the other end of an already-established connection
        # answers query-migrate with an error dict (see cmd()'s own
        # docstring) -- that must be terminal, not a None status polled for
        # the full 600s timeout. Proven by call count, not wall time: a big
        # timeout with sleep=0 would return "fast" either way, but a real
        # bug here would still send hundreds of query-migrate commands.
        fake = FakeQmp([{"return": {}},
                        {"error": {"class": "GenericError", "desc": "gone"}}])
        with QmpSession(fake.port) as s:
            status = s.wait_migrate(timeout=600.0, sleep=0)
        self.assertEqual(status, "failed")
        qm_calls = [m for m in fake.received if m["execute"] == "query-migrate"]
        self.assertEqual(len(qm_calls), 1)

    def test_wait_migrate_treats_a_closed_connection_as_terminal_not_a_hang(self):
        # Exactly the corrupt-state-file case the design anticipates: QEMU
        # exits while loading, the socket goes dead, and cmd() degrades that
        # to an error dict rather than raising. wait_migrate must not spin.
        fake = FakeQmp([{"return": {}}])
        s = QmpSession(fake.port)
        try:
            s._sock.shutdown(socket.SHUT_WR)
            status = s.wait_migrate(timeout=600.0, sleep=0)
            self.assertEqual(status, "failed")
        finally:
            s.close()

    def test_connect_failure_raises_a_clear_error(self):
        # Port 1 is never a QMP server; the caller must be able to catch this
        # and fall back to a cold boot.
        with self.assertRaises(OSError):
            QmpSession(1, connect_timeout=0.5)

    def test_cmd_skips_an_unsolicited_event_and_returns_the_real_reply(self):
        # QMP interleaves async events with command replies. Only a reply
        # carries return/error; an event mistaken for a reply desyncs every
        # later command onto the previous command's leftover line.
        fake = FakeQmp([
            {"return": {}},
            [
                {"event": "STOP", "timestamp": {"seconds": 0, "microseconds": 0}},
                {"return": {"status": "running", "running": True}},
            ],
        ])
        with QmpSession(fake.port) as s:
            r = s.cmd("query-status")
        self.assertEqual(r, {"return": {"status": "running", "running": True}})

    def test_socket_is_closed_when_the_greeting_never_arrives(self):
        # QEMU accepted the TCP connection but died/hung before greeting.
        # The handshake must fail loudly AND close the socket -- otherwise
        # every retried cold-boot fallback leaks another fd.
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        port = srv.getsockname()[1]

        def accept_and_die():
            conn, _ = srv.accept()
            conn.close()  # no greeting, ever

        threading.Thread(target=accept_and_die, daemon=True).start()
        try:
            with _spy_created_sockets() as created:
                with self.assertRaises(OSError):
                    QmpSession(port, connect_timeout=2.0, timeout=2.0)
            self.assertEqual(len(created), 1)
            self.assertEqual(created[0].fileno(), -1)  # closed, not leaked
        finally:
            srv.close()

    def test_error_reply_to_capabilities_raises_and_closes_the_socket(self):
        # If capability negotiation itself errors, construction must not
        # "succeed" silently -- every later command would misbehave.
        fake = FakeQmp([{"error": {"class": "GenericError", "desc": "nope"}}])
        with _spy_created_sockets() as created:
            with self.assertRaises(OSError):
                QmpSession(fake.port, connect_timeout=2.0, timeout=2.0)
        self.assertEqual(len(created), 1)
        self.assertEqual(created[0].fileno(), -1)  # closed, not leaked

    def test_cmd_returns_an_error_dict_when_the_local_write_side_is_broken(self):
        # A dead QEMU on the other end must degrade cmd() to an error dict,
        # not an uncaught BrokenPipeError -- callers fall back to cold boot
        # on an error dict, they cannot catch an arbitrary exception.
        fake = FakeQmp([{"return": {}}])
        s = QmpSession(fake.port)
        s._sock.shutdown(socket.SHUT_WR)
        try:
            result = s.cmd("query-status")
            self.assertIn("error", result)
        finally:
            s.close()


if __name__ == "__main__":
    unittest.main()
