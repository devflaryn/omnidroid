#!/usr/bin/env python3
"""Which golden entries are currently backing a RUNNING instance.

    python3 -m pytest tests/test_warm_in_use.py -q

Two consumers depend on this: eviction (never delete the disks a live
instance is backed by) and the interim concurrency rule (a second launch
against an in-use entry must cold-boot, because a second concurrent restore
lands `offline` on adb -- see the design spec section 8b).
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import runtime  # noqa: E402


class InUseKeys(unittest.TestCase):
    def setUp(self):
        self.tmp = os.environ.get("OMNI_DATA_DIR")

    def _run(self, tmp_path, name, **fields):
        d = tmp_path / "runtime" / name
        d.mkdir(parents=True, exist_ok=True)
        (d / "run.json").write_text(json.dumps(fields))

    def test_collects_keys_from_live_instances(self, ):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self._run(tmp, "a", pid=os.getpid(), warm_key="k1")
        self._run(tmp, "b", pid=os.getpid(), warm_key="k2")

        self.assertEqual(runtime.warm_keys_in_use(), {"k1", "k2"})

    def test_instances_without_a_warm_key_contribute_nothing(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self._run(tmp, "cold", pid=os.getpid())

        self.assertEqual(runtime.warm_keys_in_use(), set())

    def test_a_dead_instance_does_not_hold_its_entry_hostage(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        # PID 1 is not one of ours; running_pid() must reject it.
        self._run(tmp, "dead", pid=999999, warm_key="ghost")

        self.assertEqual(runtime.warm_keys_in_use(), set())

    def test_missing_runtime_dir_is_empty_not_an_error(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self.assertEqual(runtime.warm_keys_in_use(), set())


if __name__ == "__main__":
    unittest.main()
