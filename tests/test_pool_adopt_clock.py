"""Adopting a warm slot must fix the guest clock before anything uses it.

`resync_guest_clock` existed for a year with exactly ONE call site -- the
warm-restore branch of `_ensure_booted`, which is unreachable on Windows
because WHPX blocks migration. So on the platform the product ships on,
nothing ever touched the guest clock.

A pool slot is a live VM, so its clock normally ticks along and the answer is
"nothing to do". The case a desktop actually has is the host SLEEPING: the
guest comes back behind by however long the lid was shut, and Roblox rejects a
badly-skewed clock at auth and at TLS with a symptom INDISTINGUISHABLE from a
dead cookie. Finding that out from the login screen is the failure this
prevents.

The first version of this wiring shipped a `NameError` -- `warmboot` is
imported per-call in this module, not at the top -- and the pool's own
`except Exception` guard swallowed it into "warm pool unavailable (name
'warmboot' is not defined); booting normally". Every launch silently cold-
booted while the pool sat full. That is what the last test here pins.
"""

import unittest
from unittest import mock

from omnidroid import engine


def _slot_rec(slot="_pool0"):
    return {"slot": slot, "ready_at": 1.0, "key": "k",
            "spec": {"mode": "farming"}}


class AdoptionResyncsTheClock(unittest.TestCase):
    def setUp(self):
        self.acct = {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
                     "vnc_port": 18001, "base": "x86"}
        self.stack = mock.patch.multiple(
            engine,
            pool_key_for=mock.DEFAULT, adb_connect=mock.DEFAULT,
            adb_getprop=mock.DEFAULT, resolve_root_shell=mock.DEFAULT,
            _wipe_runtime=mock.DEFAULT)
        self.m = self.stack.start()
        self.addCleanup(self.stack.stop)
        self.m["pool_key_for"].return_value = "k"
        self.m["adb_getprop"].return_value = "1"
        self.m["resolve_root_shell"].return_value = ""

    def _adopt(self, clock_result):
        with mock.patch.object(engine.pool, "claim",
                               return_value=_slot_rec()), \
             mock.patch.object(engine.pool, "adopt_run_json",
                               return_value={"pid": 1}), \
             mock.patch.object(engine.pool, "acct_from_slot",
                               return_value=self.acct), \
             mock.patch.object(engine.pool, "mark_adopted"), \
             mock.patch("omnidroid.warmboot.resync_guest_clock",
                        return_value=clock_result) as clock:
            got = engine.pool_try_adopt("u1", {}, {"mode": "farming"}, "lbl")
        return got, clock

    def test_the_clock_is_resynced_on_every_adoption(self):
        got, clock = self._adopt({"skew_s": 0, "corrected": False,
                                  "reason": "within_threshold",
                                  "residual_s": None})
        self.assertIs(got, self.acct)
        clock.assert_called_once()
        self.assertEqual(clock.call_args.args[0], self.acct)

    def test_it_uses_the_root_shell_this_base_actually_has(self):
        """The x86 Bliss base has no `su` binary and its adbd runs as uid 0,
        so the root mode is `""`. A bare `su -c` would fail on every launch."""
        _got, clock = self._adopt({"skew_s": 0, "corrected": False,
                                   "reason": "within_threshold",
                                   "residual_s": None})
        self.assertIs(clock.call_args.kwargs.get("root_fn"),
                      self.m["resolve_root_shell"])

    def test_a_correction_that_did_not_take_is_said_out_loud(self):
        """A residual after a correction is Android putting the clock back --
        and it is the one shape of this failure a user could otherwise only
        diagnose as 'my cookie died'."""
        with mock.patch("builtins.print") as out:
            self._adopt({"skew_s": 900, "corrected": True,
                         "reason": "corrected", "residual_s": 880})
        said = " ".join(str(c.args[0]) for c in out.call_args_list if c.args)
        self.assertIn("did NOT take", said)

    def test_a_quiet_resync_says_nothing_extra(self):
        with mock.patch("builtins.print") as out:
            self._adopt({"skew_s": 0, "corrected": False,
                         "reason": "within_threshold", "residual_s": None})
        said = " ".join(str(c.args[0]) for c in out.call_args_list if c.args)
        self.assertNotIn("did NOT take", said)
        self.assertIn("warm pool", said)


class AClaimedSlotIsAlwaysReleased(unittest.TestCase):
    """`cmd_start` catches everything this raises and boots normally, which
    fixes the LAUNCH and abandons the SLOT. A throw between the claim and the
    return leaves `adopted.json` on disk forever: a live, healthy, ready
    instance that will never be handed to anybody again, while `pool status`
    reports it adopted by an account that is not using it.

    Measured: one NameError in the clock step burned the only slot in the pool,
    and every launch after it cold-booted while the pool reported itself full.
    """

    def setUp(self):
        self.acct = {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
                     "vnc_port": 18001, "base": "x86"}
        self.stack = mock.patch.multiple(
            engine,
            pool_key_for=mock.DEFAULT, adb_connect=mock.DEFAULT,
            adb_getprop=mock.DEFAULT, resolve_root_shell=mock.DEFAULT,
            _wipe_runtime=mock.DEFAULT)
        self.m = self.stack.start()
        self.addCleanup(self.stack.stop)
        self.m["pool_key_for"].return_value = "k"
        self.m["adb_getprop"].return_value = "1"
        self.m["resolve_root_shell"].return_value = ""

    def test_a_throw_after_the_claim_releases_the_slot(self):
        with mock.patch.object(engine.pool, "claim",
                               return_value=_slot_rec()), \
             mock.patch.object(engine.pool, "adopt_run_json",
                               return_value={"pid": 1}), \
             mock.patch.object(engine.pool, "acct_from_slot",
                               return_value=self.acct), \
             mock.patch.object(engine.pool, "mark_adopted"), \
             mock.patch.object(engine.pool, "release") as release, \
             mock.patch("omnidroid.warmboot.resync_guest_clock",
                        side_effect=NameError("warmboot is not defined")):
            with self.assertRaises(NameError):
                engine.pool_try_adopt("u1", {}, {"mode": "farming"}, "lbl")
        release.assert_called_once()
        self.assertEqual(release.call_args.args[0], "_pool0")

    def test_the_runtime_dir_is_wiped_too(self):
        """adopt_run_json already wrote `runtime/<name>/run.json` pointing at
        the slot's QEMU. Leaving it behind makes the cold boot that follows
        find a `run.json` for a process it does not own."""
        with mock.patch.object(engine.pool, "claim",
                               return_value=_slot_rec()), \
             mock.patch.object(engine.pool, "adopt_run_json",
                               return_value={"pid": 1}), \
             mock.patch.object(engine.pool, "acct_from_slot",
                               return_value=self.acct), \
             mock.patch.object(engine.pool, "mark_adopted"), \
             mock.patch.object(engine.pool, "release"), \
             mock.patch("omnidroid.warmboot.resync_guest_clock",
                        side_effect=RuntimeError("boom")):
            with self.assertRaises(RuntimeError):
                engine.pool_try_adopt("u1", {}, {"mode": "farming"}, "lbl")
        self.m["_wipe_runtime"].assert_called_with("u1")

    def test_a_clean_adoption_releases_nothing(self):
        with mock.patch.object(engine.pool, "claim",
                               return_value=_slot_rec()), \
             mock.patch.object(engine.pool, "adopt_run_json",
                               return_value={"pid": 1}), \
             mock.patch.object(engine.pool, "acct_from_slot",
                               return_value=self.acct), \
             mock.patch.object(engine.pool, "mark_adopted"), \
             mock.patch.object(engine.pool, "release") as release, \
             mock.patch("omnidroid.warmboot.resync_guest_clock",
                        return_value={"skew_s": 0, "corrected": False,
                                      "reason": "within_threshold",
                                      "residual_s": None}):
            got = engine.pool_try_adopt("u1", {}, {"mode": "farming"}, "lbl")
        self.assertIs(got, self.acct)
        release.assert_not_called()


class TheWiringItselfResolves(unittest.TestCase):
    def test_pool_try_adopt_can_reach_warmboot(self):
        """The regression: `warmboot` is imported PER-CALL in engine.py, and
        the first version of this wiring referenced it as a module global.

        The pool wraps adoption in `except Exception`, so the NameError became
        'warm pool unavailable; booting normally' -- a full pool, every launch
        cold-booting, and a message that reads like a design decision. Asserted
        by source, because a mocked warmboot is exactly what hides it."""
        import inspect
        src = inspect.getsource(engine._pool_adopt_claimed)
        self.assertIn("from omnidroid import warmboot", src)
        self.assertIn("resync_guest_clock", src)
        self.assertLess(src.index("from omnidroid import warmboot"),
                        src.index("resync_guest_clock"))

    def test_the_claimed_half_is_only_reachable_through_the_guard(self):
        """`_pool_adopt_claimed` exists so the release guard cannot be walked
        around. If something ever calls it directly, a throw stops releasing
        again and the bug this file documents comes straight back."""
        import inspect
        engine_src = inspect.getsource(engine)
        calls = engine_src.count("_pool_adopt_claimed(")
        self.assertEqual(calls, 2, "def + exactly one call site")
        self.assertIn("return _pool_adopt_claimed(rec, name, label)",
                      inspect.getsource(engine.pool_try_adopt))


if __name__ == "__main__":
    unittest.main()
