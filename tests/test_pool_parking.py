#!/usr/bin/env python3
"""A warm slot is held at its idle cost, and gets its memory back when taken.

    python3 -m pytest tests/test_pool_parking.py -q

THE GAP THIS CLOSES. `maybe_start_governor` is called from `cmd_start` and
from nowhere else, so a pool slot -- the one instance in this engine that is
deliberately kept running while doing nothing -- was the one instance nothing
governed. MEASURED 2026-08-17 across five warm slots: 575-747 MB of host RSS
each, i.e. 3.4 GB held for five guests with no game in any of them.

A slot does NOT get the governor, and that is a design decision rather than an
omission. The governor SEARCHES a ceiling down while watching a client,
because the safe floor is a property of the game and getting it wrong kills
the client. A slot has no client: nothing to search for, nothing to lose. So
the floor the mode already names is applied directly, once.

THE PAIR IS THE POINT. Parking without releasing would leave an idle-sized
ceiling on a guest about to fault in ~1.5 GB of Roblox -- which is the
"squeezed while it was still loading" failure this project already paid for
once (see CHANGELOG 2026-08-16). Every test below exists because half of this
mechanism is worse than none of it.
"""
import inspect
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine, qemu_proc


class AReadySlotIsParked(unittest.TestCase):

    def _park(self, mode_name="farming", pid=4242, rss=700):
        cfg = {"bases": {"x86": {"type": "x86-bliss"}}, "current_base": "x86"}
        with mock.patch.object(engine, "running_pid", return_value=pid), \
             mock.patch.object(engine, "IS_WINDOWS", True), \
             mock.patch.object(engine, "host_rss_mb", return_value=rss), \
             mock.patch.object(engine, "_select_base_tag", return_value="x86"), \
             mock.patch.object(engine, "arch_of_base", return_value="x86"), \
             mock.patch.object(engine, "cap_working_set",
                               return_value=True) as cap, \
             mock.patch.object(engine.pool, "write_slot_meta") as meta:
            got = engine.park_slot("_pool0", {"mode": mode_name}, cfg)
        return got, cap, meta

    def test_a_farming_slot_is_held_at_the_modes_own_floor(self):
        """Not a number invented here: the same `ws_floor` the governor's
        search is allowed to reach, so a parked slot and a governed instance
        cannot disagree about what farming's floor is."""
        got, cap, _meta = self._park()
        floor = qemu_proc.MODES["farming"]["ws_floor"]
        self.assertEqual(got, floor)
        cap.assert_called_once_with(4242, floor)

    def test_the_ceiling_is_recorded_on_the_slot(self):
        """`pool status` has to be able to say what a slot costs. A cap that
        is applied but not written down is indistinguishable from no cap when
        somebody is trying to work out where the memory went."""
        _got, _cap, meta = self._park(rss=700)
        self.assertEqual(meta.call_args.kwargs["ws_ceiling_mb"],
                         qemu_proc.MODES["farming"]["ws_floor"])
        self.assertEqual(meta.call_args.kwargs["ws_before_mb"], 700)

    def test_a_mode_with_no_floor_is_left_alone(self):
        """Gaming names no `ws_floor` because a gaming instance is supposed to
        be expensive. Parking one would be this mechanism reaching past what
        it was measured for."""
        got, cap, _meta = self._park(mode_name="gaming")
        self.assertIsNone(got)
        cap.assert_not_called()

    def test_a_slot_whose_qemu_is_gone_is_not_capped(self):
        """A pid that is not there is a recycled pid waiting to happen, and
        the thing being capped is somebody else's process."""
        got, cap, _meta = self._park(pid=None)
        self.assertIsNone(got)
        cap.assert_not_called()


class ParkingHappensBeforeTheSlotCanBeClaimed(unittest.TestCase):

    def test_park_is_called_before_the_ready_write(self):
        """THE RACE. From the `ready` write onwards any `cmd_start` may claim
        the slot. Parking after it would sometimes cap a guest that had just
        been adopted and was already loading a game -- the precise thing
        adoption's uncap exists to prevent, arriving from the other side.

        Asserted by source order: the window is short enough that no test
        which actually races the two would fail reliably, and a flaky test
        for a real race is how the race gets marked as flaky and ignored."""
        src = inspect.getsource(engine.pool_boot_slot)
        park = src.index("park_slot(")
        ready = src.index('state="ready"')
        self.assertLess(park, ready,
                        "a slot must never be claimable while uncapped")


class AdoptionGivesTheMemoryBack(unittest.TestCase):

    def test_adoption_uncaps_before_the_session_is_delivered(self):
        """The client is about to fault in ~1.5 GB. Whatever ceiling held the
        slot idle is the wrong number for that, and the real governor will
        re-derive one from the instance this now IS.

        Source-asserted for the same reason as the clock's own test in
        test_pool_adopt_clock.py: mocking the thing that must be called is
        exactly what hides its absence."""
        src = inspect.getsource(engine._pool_adopt_claimed)
        self.assertIn("uncap_working_set", src)
        self.assertLess(src.index("uncap_working_set"),
                        src.index("resync_guest_clock"),
                        "give the memory back before anything else touches "
                        "the guest")

    def test_the_uncap_is_guarded_on_a_recorded_pid(self):
        """An adopted run.json written before this existed has no `pid` for
        the same reason it has no `pid_started`. Reaching into it unguarded
        would turn an upgrade into a traceback on the fast path."""
        src = inspect.getsource(engine._pool_adopt_claimed)
        self.assertIn('run.get("pid")', src)


if __name__ == "__main__":
    unittest.main()
