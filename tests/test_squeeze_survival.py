"""A density launch must not report success for a client that is already dying.

MEASURED 2026-08-15, PS99, farming, four runs on two different render paths:

    full squeeze, 3 GB, GPU        alive t+0/30/60,  DEAD t+90
    full squeeze, 3 GB, software   alive t+0/30/60,  DEAD t+90   (control)
    full squeeze, 4 GB             alive to t+151,   DEAD t+181
    whole squeeze SKIPPED          alive at t+421, still going

The two squeezed runs died within 0.05 s of each other across completely
different renderers, so it is a timer rather than a crash. And the 4 GB run
died with 1604 MB still available, so it is not exhaustion either.

What makes it a REPORTING bug as well as an engine bug: every check the launch
performs -- `ok`, `delivered`, `played`, and `probe_client_join`'s `in_world`
-- runs inside that first minute. So the launch cheerfully returns
`ok: true, in_world: true` about an instance with about a minute to live, which
is why this went unnoticed for the entire life of the mode.
"""

import os
import subprocess
import unittest
from unittest import mock

from omnidroid import engine


class _Out:
    def __init__(self, stdout):
        self.stdout = stdout


class GraceBudget(unittest.TestCase):
    def test_env_wins_and_zero_disables(self):
        with mock.patch.dict(os.environ, {"OMNI_SQUEEZE_GRACE": "45"}):
            self.assertEqual(engine.squeeze_grace_s(), 45)
        with mock.patch.dict(os.environ, {"OMNI_SQUEEZE_GRACE": "0"}):
            self.assertEqual(engine.squeeze_grace_s(), 0)

    def test_config_is_the_fallback(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("OMNI_SQUEEZE_GRACE", None)
            self.assertEqual(
                engine.squeeze_grace_s({"qemu": {"squeeze_grace": 200}}), 200)

    def test_nonsense_falls_back_to_the_default(self):
        with mock.patch.dict(os.environ, {"OMNI_SQUEEZE_GRACE": "soon"}):
            self.assertEqual(engine.squeeze_grace_s(),
                             engine.SQUEEZE_GRACE_S)


class Verdicts(unittest.TestCase):
    def _run(self, adb_result, grace="30"):
        kw = ({"side_effect": adb_result}
              if isinstance(adb_result, Exception) else
              {"return_value": adb_result})
        with mock.patch.object(engine, "adb", **kw), \
             mock.patch.dict(os.environ, {"OMNI_SQUEEZE_GRACE": grace}), \
             mock.patch.object(engine.time, "sleep"):
            return engine.verify_client_survived({"name": "x"}, None)

    def test_an_empty_pidof_is_a_dead_client(self):
        r = self._run(_Out(""))
        self.assertIs(r["alive"], False)
        self.assertEqual(r["reason"], "process_gone")

    def test_a_live_pid_outlives_the_grace(self):
        r = self._run(_Out("4321\n"), grace="1")
        self.assertIs(r["alive"], True)
        self.assertEqual(r["reason"], "outlived_grace")

    def test_zero_grace_is_reported_as_disabled_not_as_alive(self):
        """`alive: None` and `alive: True` are different claims. A launch that
        did not look must not read as a launch that looked and was happy."""
        r = self._run(_Out(""), grace="0")
        self.assertIsNone(r["alive"])
        self.assertEqual(r["reason"], "disabled")

    def test_a_flaky_adb_is_not_a_dead_game(self):
        r = self._run(OSError("device offline"), grace="1")
        self.assertIs(r["alive"], True)

    def test_it_never_raises_into_the_launch(self):
        for boom in (OSError("x"), subprocess.SubprocessError("y"),
                     ValueError("z")):
            self._run(boom, grace="1")


class TheCheckCannotSilentlyPass(unittest.TestCase):
    """The first version of this caught bare `Exception` and referenced an
    undefined `GAME_PKG`. Every call raised NameError, was swallowed, and
    reported the client ALIVE -- a liveness check that could not fail, which
    is the one thing it must never be. Both halves are pinned here."""

    def test_it_uses_a_name_that_actually_resolves(self):
        import inspect
        src = inspect.getsource(engine.verify_client_survived)
        self.assertIn("farming.GAME_PKG", src)
        self.assertNotIn("pidof\", GAME_PKG", src)

    def test_the_except_is_narrow_enough_to_let_a_bug_through(self):
        import inspect
        src = inspect.getsource(engine.verify_client_survived)
        self.assertNotIn("except Exception", src)

    def test_a_programming_error_is_not_swallowed(self):
        with mock.patch.object(engine, "adb", side_effect=NameError("boom")), \
             mock.patch.dict(os.environ, {"OMNI_SQUEEZE_GRACE": "5"}), \
             mock.patch.object(engine.time, "sleep"):
            with self.assertRaises(NameError):
                engine.verify_client_survived({"name": "x"}, None)


class TheStageIsDeclared(unittest.TestCase):
    def test_density_declares_both_of_its_stages(self):
        stages = engine._start_timings_stages(False, density=True)
        self.assertIn("density_settled", stages)
        self.assertIn("squeeze_verified", stages)
        self.assertLess(stages.index("density_settled"),
                        stages.index("squeeze_verified"))

    def test_a_performance_launch_declares_neither(self):
        stages = engine._start_timings_stages(False, density=False)
        self.assertNotIn("density_settled", stages)
        self.assertNotIn("squeeze_verified", stages)


if __name__ == "__main__":
    unittest.main()
