# omnidroid/tests/test_output_dead_stream.py
"""Narration must never be able to kill a command.

OBSERVED on Windows: the GUI runs the engine as a child and reads its
stdout/stderr through pipes. When the app stopped reading -- its watchdog
timer fired after a launch stalled -- the engine's NEXT progress line hit a
dead pipe and raised OSError(EINVAL, "Invalid argument"). In the frozen
windowed build that is a PyInstaller "Unhandled exception in script" dialog
and a dead launch:

    File "omnidroid\\engine.py", line 596, in wait_for_boot
    File "omnidroid\\output.py", line 30, in _to_stderr
    OSError: [Errno 22] Invalid argument

Losing the message is the correct trade. The instance is fine; only the
narration is gone.
"""
import io
import sys
import unittest

from omnidroid import output


class DeadStream(io.StringIO):
    """A stream whose writes fail the way a dead Windows pipe does."""

    def __init__(self, exc=None):
        super().__init__()
        self._exc = exc or OSError(22, "Invalid argument")

    def write(self, *_a, **_k):
        raise self._exc

    def flush(self):
        raise self._exc


class WriteSafely(unittest.TestCase):
    def test_a_live_stream_is_written_and_reported_true(self):
        s = io.StringIO()
        self.assertTrue(output.write_safely(s, "hello\n"))
        self.assertEqual(s.getvalue(), "hello\n")

    def test_a_dead_pipe_is_swallowed(self):
        self.assertFalse(output.write_safely(DeadStream(), "x"))

    def test_a_closed_file_is_swallowed(self):
        s = io.StringIO()
        s.close()
        self.assertFalse(output.write_safely(s, "x"))   # ValueError

    def test_a_none_stream_is_swallowed(self):
        # A frozen windowed process can have no stdio at all.
        self.assertFalse(output.write_safely(None, "x"))

    def test_a_stub_without_write_is_swallowed(self):
        self.assertFalse(output.write_safely(object(), "x"))


class JsonModePrinting(unittest.TestCase):
    def setUp(self):
        self._print = __import__("builtins").print
        self._json_mode = output._JSON_MODE
        self._stderr = sys.stderr

    def tearDown(self):
        import builtins
        builtins.print = self._print
        output._JSON_MODE = self._json_mode
        sys.stderr = self._stderr

    def test_progress_printing_survives_a_dead_stderr(self):
        # THE regression: this is the exact path wait_for_boot takes.
        output.enable_json_mode()
        sys.stderr = DeadStream()
        print("[start u1] 0.5 min - Android booting", flush=True)  # must not raise

    def test_progress_still_reaches_a_live_stderr(self):
        output.enable_json_mode()
        sys.stderr = io.StringIO()
        print("[start u1] progress")
        self.assertIn("progress", sys.stderr.getvalue())


class JsonPayload(unittest.TestCase):
    def setUp(self):
        self._stdout = sys.stdout

    def tearDown(self):
        sys.stdout = self._stdout

    def test_emit_json_survives_a_dead_stdout(self):
        sys.stdout = DeadStream()
        output.emit_json({"ok": True})          # must not raise

    def test_emit_json_writes_one_line_when_alive(self):
        sys.stdout = io.StringIO()
        output.emit_json({"ok": True})
        self.assertEqual(sys.stdout.getvalue(), '{"ok": true}\n')


if __name__ == "__main__":
    unittest.main()
