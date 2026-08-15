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
from unittest import mock  # noqa: E402
from omnidroid import migfile, warmboot, warmcache  # noqa: E402


def _file_transport():
    """Pin these tests to the `file:` transport.

    warmboot picks its transport per platform now (migfile.default_transport):
    `file:` where QEMU can migrate to a file, and a TCP relay on Windows, where
    it cannot. FakeSession models the `file:` side effect, so pinning keeps this
    class about ORDER and FAILURE HANDLING on every host. The relay itself is
    covered end to end in TcpRelay below, against a real socket."""
    return mock.patch.object(migfile, "default_transport",
                             return_value=migfile.TRANSPORT_FILE)


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

    def set_migration_caps(self, channels=4, caps=None):
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


class _R:
    """An adb CompletedProcess, as much of one as this module reads."""

    def __init__(self, out=""):
        self.stdout = out


def _fake_adb(guest_epochs, sent):
    """An adb that answers `date +%s` from `guest_epochs` (in order, the last
    value repeating) and records every argv it was handed.

    Recording ARGV is the whole point: these tests assert on the command that
    would be sent, never by running adb. The guest shell re-parses whatever
    adb joins with spaces, so the exact element boundaries are the behaviour.
    """
    seq = list(guest_epochs)

    def adb(acct, *args, **kw):
        sent.append(args)
        if args[:2] == ("shell", "date"):
            return _R(f"{seq.pop(0) if len(seq) > 1 else seq[0]}\n")
        return _R("")
    return adb


def _set_calls(sent):
    return [" ".join(a) for a in sent if "date -s" in " ".join(a)]


class ClockResync(unittest.TestCase):
    """A restored guest wakes with the clock frozen at bake time; measured
    skew equals the wall time since the bake. -rtc base=utc,clock=host does
    NOT fix it. A wrong clock breaks TLS and cookie acceptance, which looks
    exactly like "auto-login is broken".

    The same function now runs on the warm-POOL path too, where the guest has
    been live for hours and the skew comes from the HOST sleeping rather than
    from a freeze -- so the no-op case matters as much as the correcting one.
    """

    def test_it_sets_the_guest_clock_to_host_time(self):
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 2000], sent),
            now_fn=lambda: 2000)

        self.assertEqual(res["skew_s"], 1000)
        self.assertTrue(res["corrected"])
        self.assertEqual(res["reason"], warmboot.CLOCK_OK)
        joined = [" ".join(a) for a in sent]
        self.assertTrue(any("date -s @2000" in j for j in joined), joined)

    def test_a_clock_already_right_costs_one_round_trip_and_no_log(self):
        # This runs on EVERY launch now. Correcting a 1s skew would spend a
        # second adb round trip and a log line per launch to move a clock that
        # was already right -- which is how a real diagnostic turns into noise
        # nobody reads.
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1999], sent),
            now_fn=lambda: 2000)

        self.assertFalse(res["corrected"])
        self.assertEqual(res["reason"], warmboot.CLOCK_WITHIN_THRESHOLD)
        self.assertEqual(res["skew_s"], 1)
        self.assertEqual(len(sent), 1, sent)
        self.assertEqual(_set_calls(sent), [])

    def test_the_threshold_is_a_parameter_not_a_constant(self):
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 2000], sent),
            now_fn=lambda: 2000, threshold_s=5000)
        self.assertEqual(res["reason"], warmboot.CLOCK_WITHIN_THRESHOLD)

    def test_root_via_adbd_sends_sh_dash_c_with_no_su_prefix(self):
        # `""` is a VALID root mode: the x86 Bliss base ships no su binary at
        # all, but its adbd already runs as uid 0. Prefixing `su` there is
        # exactly what made every root-gated tune skip on x86 -- and a clock
        # left uncorrected is reported to the user as a dead cookie.
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 2000], sent),
            now_fn=lambda: 2000, root_fn=lambda acct: "")

        self.assertTrue(res["corrected"])
        self.assertEqual(_set_calls(sent), ["shell sh -c 'date -s @2000'"])

    def test_a_su_binary_gets_the_magisk_argv_form(self):
        sent = []
        warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 2000], sent),
            now_fn=lambda: 2000, root_fn=lambda acct: "/sbin/su")
        self.assertEqual(_set_calls(sent),
                         ["shell /sbin/su 0 sh -c 'date -s @2000'"])

    def test_no_root_reports_it_instead_of_raising_or_pretending(self):
        # None means "no root at all". Sending the command anyway would exit 0
        # with the clock untouched, and this would log a successful resync
        # that never happened.
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000], sent),
            now_fn=lambda: 2000, root_fn=lambda acct: None)

        self.assertFalse(res["corrected"])
        self.assertEqual(res["reason"], warmboot.CLOCK_NO_ROOT)
        self.assertEqual(res["skew_s"], 1000)
        self.assertEqual(_set_calls(sent), [])

    def test_a_root_resolver_that_blows_up_is_no_root_not_a_failed_boot(self):
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000], []),
            now_fn=lambda: 2000,
            root_fn=lambda acct: (_ for _ in ()).throw(RuntimeError("boom")))
        self.assertEqual(res["reason"], warmboot.CLOCK_NO_ROOT)

    def test_no_resolver_keeps_the_legacy_su_dash_c_form(self):
        sent = []
        warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 2000], sent),
            now_fn=lambda: 2000)
        self.assertEqual(_set_calls(sent), ["shell su -c date -s @2000"])

    def test_a_correction_that_does_not_stick_is_reported_not_celebrated(self):
        # `date -s` exits 0 and the clock does not move: Android's time
        # detector re-applying its own network suggestion is the leading
        # suspect. Whether that happens on these bases is unverified, so the
        # residual read is how the measurement gets made in the field.
        sent = []
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=_fake_adb([1000, 1000], sent),
            now_fn=lambda: 2000, root_fn=lambda acct: "")

        self.assertFalse(res["corrected"])
        self.assertEqual(res["reason"], warmboot.CLOCK_SET_FAILED)
        self.assertEqual(res["residual_s"], 1000)

    def test_a_failing_set_command_still_returns_a_result(self):
        def adb(acct, *args, **kw):
            if args[:2] == ("shell", "date"):
                return _R("1000\n")
            raise OSError("device offline")

        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=adb, now_fn=lambda: 2000)
        self.assertEqual(res["reason"], warmboot.CLOCK_SET_FAILED)
        self.assertEqual(res["skew_s"], 1000)

    def test_unreadable_guest_clock_returns_a_result_instead_of_raising(self):
        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=lambda *a, **k: _R("not-a-number"),
            now_fn=lambda: 1)
        self.assertEqual(res["reason"], warmboot.CLOCK_UNREADABLE)
        # None skew is NOT zero skew: a caller must not read this as "fine".
        self.assertIsNone(res["skew_s"])
        self.assertFalse(res["corrected"])

    def test_adb_timeout_reading_the_clock_is_a_result_not_an_exception(self):
        # The real adb() is subprocess.run(..., timeout=...), which raises
        # subprocess.TimeoutExpired -- NOT an OSError -- when a still-booting
        # guest never answers. That is the single most likely real-world
        # cause of an unreadable clock, so it must degrade like every other
        # failure here rather than propagate into the boot path.
        def timing_out(*a, **k):
            raise subprocess.TimeoutExpired(cmd="adb shell date +%s", timeout=20)

        res = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=timing_out, now_fn=lambda: 1)
        self.assertEqual(res["reason"], warmboot.CLOCK_UNREADABLE)
        self.assertIsNone(res["skew_s"])


class ClockProbe(unittest.TestCase):
    """The read-only half. A caller has to be able to ASK how far off a guest
    is -- e.g. to decide whether a long-warm pool slot is worth recycling --
    without touching a guest somebody may be playing on."""

    def test_it_reads_the_skew_and_mutates_nothing(self):
        sent = []
        skew = warmboot.guest_clock_skew(
            {"name": "t"}, adb_fn=_fake_adb([1000], sent), now_fn=lambda: 2000)

        self.assertEqual(skew, 1000)
        self.assertEqual(sent, [("shell", "date", "+%s")])

    def test_an_unreadable_clock_is_none_not_zero(self):
        # Zero would read as "this guest is fine", which is the opposite of
        # what an unanswerable adb means.
        self.assertIsNone(warmboot.guest_clock_skew(
            {"name": "t"}, adb_fn=lambda *a, **k: _R(""), now_fn=lambda: 1))

    def test_adb_noise_before_the_answer_is_not_an_unreadable_clock(self):
        # adb interleaves its own chatter with command output; engine's own
        # `id -u` parsing takes the last line for the same reason.
        noisy = _R("* daemon started successfully *\n1000\n")
        self.assertEqual(warmboot.guest_clock_skew(
            {"name": "t"}, adb_fn=lambda *a, **k: noisy,
            now_fn=lambda: 2000), 1000)


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
        with _file_transport():
            ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                       "lbl",
                                       session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        order = [c for c in sess.calls
                 if c in ("migrate-set-capabilities", "migrate-incoming",
                          "wait_migrate", "cont")]
        self.assertEqual(order, ["migrate-set-capabilities", "migrate-incoming",
                                 "wait_migrate", "cont"])

    def test_failed_migration_reports_false_and_never_conts(self):
        sess = FakeSession(0, migrate_status="failed")
        with _file_transport():
            ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                       "lbl",
                                       session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertNotIn("cont", sess.calls)

    def test_qmp_that_never_answers_is_false_not_an_exception(self):
        def boom(*a, **k):
            raise OSError("no QMP")

        self.assertFalse(warmboot.restore_into(
            {"name": "t", "qmp_port": 1}, self.entry, "lbl",
            session_factory=boom))

    def test_restore_bounds_the_connect_timeout_under_the_restore_budget(self):
        # RESTORE_TIMEOUT (engine.py) is 30s; QmpSession's own connect
        # default is 60s. Burning that before even attempting the migration
        # would blow the restore's whole budget on a QEMU that never opens
        # its QMP port -- restore_into must pass something well under 30s.
        captured = {}

        def factory(port, **kw):
            captured.update(kw)
            return FakeSession(port, **kw)

        with _file_transport():
            ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                       "lbl", session_factory=factory)
        self.assertTrue(ok)
        self.assertIn("connect_timeout", captured)
        self.assertLess(captured["connect_timeout"], 30.0)


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
        with _file_transport():
            ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                     "k", {"qemu_version": "11.0.2"}, self.rd,
                                     "lbl",
                                     session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        self.assertIsNotNone(warmcache.lookup(self.images, "k", "11.0.2"))
        # The single most safety-critical property of a bake: a resumed
        # guest would keep writing to the very overlays the state file
        # describes, silently diverging them from what was just captured.
        self.assertNotIn("cont", sess.calls)

    def test_it_stops_the_vm_before_migrating(self):
        # Migrating a running guest would capture a torn machine.
        sess = FakeSession(0)
        with _file_transport():
            warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images, "k",
                                {"qemu_version": "11.0.2"}, self.rd, "lbl",
                                session_factory=lambda *a, **k: sess)
        self.assertLess(sess.calls.index("stop"), sess.calls.index("migrate"))

    def test_failed_bake_leaves_no_entry_behind(self):
        sess = FakeSession(0, migrate_status="failed")
        with _file_transport():
            ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                     "k", {"qemu_version": "11.0.2"}, self.rd,
                                     "lbl",
                                     session_factory=lambda *a, **k: sess)
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
        with _file_transport():
            ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                     "k", {"qemu_version": "11.0.2"}, self.rd,
                                     "lbl",
                                     session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertIsNone(warmcache.lookup(self.images, "k", "11.0.2"))



class TransportChoice(unittest.TestCase):
    """QEMU cannot migrate to a FILE on Windows, so the transport is a
    platform fact, not a preference.

    MEASURED 2026-08-15 on QEMU 11.0.50 for Windows, a 256 MB throwaway guest:
    `migrate file:...` fails with "Failed to set FD nonblocking: Input/output
    error" (Windows has no non-blocking file handles), mapped-ram alone fails
    the same way, and mapped-ram + multifd killed the QEMU process outright.
    The same guest migrated to `tcp:127.0.0.1:<port>` in 0.2 s.
    """

    def test_windows_relays_over_tcp_and_asks_for_no_file_only_caps(self):
        with mock.patch.object(migfile, "IS_WINDOWS", True):
            self.assertEqual(migfile.default_transport(), migfile.TRANSPORT_TCP)
        # mapped-ram means "write each page at its own offset in the
        # destination file" -- a stream socket cannot express it, and asking
        # for it anyway is what killed QEMU.
        self.assertEqual(migfile.transport_caps(migfile.TRANSPORT_TCP), ())

    def test_elsewhere_it_uses_the_file_path_with_mapped_ram(self):
        with mock.patch.object(migfile, "IS_WINDOWS", False):
            self.assertEqual(migfile.default_transport(),
                             migfile.TRANSPORT_FILE)
        self.assertIn("mapped-ram",
                      migfile.transport_caps(migfile.TRANSPORT_FILE))

    def test_the_entry_records_which_transport_wrote_it(self):
        # The two formats are not interchangeable: a mapped-ram file fed to a
        # QEMU that did not enable the capability is rejected, and that looks
        # exactly like a corrupt entry.
        images = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, images, ignore_errors=True)
        rd = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, rd, ignore_errors=True)
        for n in ("bake_system.qcow2", "bake_data.qcow2", "efivars.fd"):
            (rd / n).write_bytes(b"x")
        sess = FakeSession(0)
        with _file_transport():
            warmboot.bake_entry({"name": "t", "qmp_port": 1}, images, "k",
                                {"qemu_version": "11.0.2"}, rd, "lbl",
                                session_factory=lambda *a, **k: sess)
        meta = warmcache.read_meta(warmcache.entry_path(images, "k"))
        self.assertEqual(meta["transport"], migfile.TRANSPORT_FILE)


class TcpRelay(unittest.TestCase):
    """The relay itself, against a REAL loopback socket.

    Faking the socket would test the mock. What has to be true is that bytes
    put in one end come out of the other, that the end of a stream is
    recognised however the peer closes it, and above all that a stream cut
    SHORT is reported as a failure rather than published as a cache entry --
    a truncated state file passes every existence check and then fails a
    restore weeks later on a machine that is not this one.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.received = None

    def _session(self, payload=b"", status="completed", incoming=False,
                 reset_after=None, transferred=None):
        """A fake QEMU end of the relay.

        `reset_after` sends only that many bytes and then RSTs the connection,
        which is what a QEMU dying mid-migration looks like on the wire.
        `transferred` is what query-migrate will claim it sent.
        """
        import socket as _socket
        import struct
        import threading

        outer = self
        claim = len(payload) if transferred is None else transferred

        class S:
            def __init__(self):
                self.calls = []
                self.t = None

            def cmd(self, execute, arguments=None):
                self.calls.append(execute)
                if execute == "query-migrate":
                    return {"return": {"status": status,
                                       "ram": {"transferred": claim}}}
                uri = (arguments or {}).get("uri", "")
                if not uri.startswith("tcp:"):
                    return {"return": {}}
                port = int(uri.rsplit(":", 1)[1])
                self.t = threading.Thread(
                    target=(self._listen if incoming else self._push),
                    args=(port,), daemon=True)
                self.t.start()
                return {"return": {}}

            def _listen(self, port):
                srv = _socket.socket()
                srv.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
                srv.bind(("127.0.0.1", port))
                srv.listen(1)
                conn, _ = srv.accept()
                buf = b""
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
                outer.received = buf
                conn.close()
                srv.close()

            def _push(self, port):
                conn = _socket.create_connection(("127.0.0.1", port),
                                                 timeout=10)
                if reset_after is None:
                    conn.sendall(payload)
                    conn.shutdown(_socket.SHUT_WR)
                else:
                    conn.sendall(payload[:reset_after])
                    # SO_LINGER {on, 0} makes close() send RST and discard
                    # anything still queued -- a hard mid-stream cut.
                    conn.setsockopt(_socket.SOL_SOCKET, _socket.SO_LINGER,
                                    struct.pack("ii", 1, 0))
                conn.close()

            def wait_migrate(self, timeout=600.0, sleep=0.25):
                if self.t is not None:
                    self.t.join(timeout=15)
                return status

        return S()

    def test_a_save_writes_exactly_what_qemu_sent(self):
        payload = bytes(range(256)) * 4096          # 1 MiB, non-trivial
        path = self.tmp / "state"
        ok, detail = migfile.save_state(self._session(payload), path,
                                        transport=migfile.TRANSPORT_TCP)
        self.assertTrue(ok, detail)
        self.assertEqual(path.read_bytes(), payload)

    def test_a_stream_cut_short_is_a_failure_not_a_cache_entry(self):
        # The whole reason _shortfall() exists. The relay reads a RST as
        # end-of-stream (it has to -- that is how a finished migration ends on
        # Windows), so without the byte cross-check this wrote a truncated
        # file and reported success.
        payload = bytes(range(256)) * 8192          # 2 MiB
        path = self.tmp / "state"
        ok, detail = migfile.save_state(
            self._session(payload, reset_after=64 * 1024), path,
            transport=migfile.TRANSPORT_TCP)
        self.assertFalse(ok)
        # Either guard may fire depending on how much of the 64 KiB survived
        # the RST -- "wrote nothing" when none of it did, "truncated" when
        # some did. What must never happen is a success.
        self.assertTrue(("truncated" in detail) or ("nothing" in detail),
                        detail)

    def test_a_qemu_that_reports_no_byte_count_is_still_accepted(self):
        # An absent counter proves nothing either way, and the bake is
        # validated by an immediate restore regardless. Refusing here would
        # turn "this QEMU reports less" into "this host has no warm cache".
        payload = b"z" * 4096
        path = self.tmp / "state"
        ok, detail = migfile.save_state(
            self._session(payload, transferred=0), path,
            transport=migfile.TRANSPORT_TCP)
        self.assertTrue(ok, detail)

    def test_a_save_that_qmp_calls_failed_is_a_failure(self):
        path = self.tmp / "state"
        ok, _ = migfile.save_state(self._session(b"x" * 1024, status="failed"),
                                   path, transport=migfile.TRANSPORT_TCP)
        self.assertFalse(ok)

    def test_a_save_that_moved_no_bytes_is_a_failure(self):
        # A zero-byte state file passes every existence check and then fails
        # the restore, which is the worst possible time to find out.
        path = self.tmp / "state"
        ok, _ = migfile.save_state(self._session(b""), path,
                                   transport=migfile.TRANSPORT_TCP)
        self.assertFalse(ok)

    def test_a_load_feeds_qemu_exactly_what_is_on_disk(self):
        payload = bytes(range(256)) * 4096
        path = self.tmp / "state"
        path.write_bytes(payload)
        ok, detail = migfile.load_state(self._session(incoming=True), path,
                                        transport=migfile.TRANSPORT_TCP)
        self.assertTrue(ok, detail)
        self.assertEqual(self.received, payload)


if __name__ == "__main__":
    unittest.main()
