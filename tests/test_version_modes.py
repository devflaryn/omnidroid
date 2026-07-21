#!/usr/bin/env python3
"""version --json exposes the engine's mode list (so GUIs derive it).

    python3 tests/test_version_modes.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


class VersionModes(unittest.TestCase):
    def test_version_report_has_modes(self):
        # cmd_version builds a report dict; capture it via the JSON path.
        import io
        import json
        from contextlib import redirect_stdout
        buf = io.StringIO()

        class A:
            json = True
        with redirect_stdout(buf):
            omni.cmd_version(A())
        rep = json.loads(buf.getvalue())
        self.assertIn("modes", rep)
        self.assertIn("farming", rep["modes"])
        self.assertEqual(set(rep["modes"]), set(omni.MODES))


if __name__ == "__main__":
    unittest.main()
