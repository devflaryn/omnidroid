#!/usr/bin/env python3
"""A host without a hypervisor must still boot.

    python3 -m pytest tests/test_accel_fallback.py -q

`default_accel()` returned `whpx,kernel-irqchip=off` on every Windows host and
nothing ever checked it was usable. "Windows Hypervisor Platform" is an OPTIONAL
Windows feature and VT-x/AMD-V is a BIOS switch, so on a PC missing either one
QEMU exited during machine init and the launch reported "QEMU exited before the
guest booted" -- with no mention of virtualization anywhere, and no way to get
the product working short of a BIOS trip the user was never told to take.

These tests pin the policy, not the numbers:

  * an accelerator is PROVEN before it is used, not assumed,
  * a host that cannot accelerate falls back to TCG and boots anyway,
  * the fallback SAYS what would make it fast again, and
  * an explicitly requested accelerator is never silently replaced without
    the user being told.

The probe itself is deliberately cheap (measured 0.06 s) and cached, because it
sits in front of every spawn.
"""
import os
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import accelprobe  # noqa: E402


def _greeting(*_a, **_k):
    return True, ""


def _dead(*_a, **_k):
    return False, "invalid accelerator whpx"


class ProbeMechanics(unittest.TestCase):
    """The probe asks QEMU, and reads the one answer that means 'it worked'."""

    def test_the_probe_command_never_touches_a_disk_or_a_screen(self):
        cmd = accelprobe.probe_command("qemu-system-x86_64", "whpx")
        self.assertIn("-nodefaults", cmd)
        self.assertIn("-S", cmd)                       # never runs the guest
        self.assertIn("none", cmd)                     # -display none
        self.assertIn("q35,accel=whpx", cmd)
        # No image, no overlay, no network: a probe must not be able to change
        # anything, and must not depend on a base being installed.
        joined = " ".join(cmd)
        for forbidden in ("-drive", "-hda", "-netdev", "-device"):
            self.assertNotIn(forbidden, joined)

    def test_a_qmp_greeting_means_the_accelerator_initialised(self):
        with mock.patch.object(accelprobe, "_qmp_greeting_seen", _greeting):
            self.assertTrue(accelprobe.probe("qemu-system-x86_64", "whpx"))

    def test_no_greeting_means_it_did_not(self):
        with mock.patch.object(accelprobe, "_qmp_greeting_seen", _dead):
            self.assertFalse(accelprobe.probe("qemu-system-x86_64", "whpx"))

    def test_a_probe_that_cannot_run_qemu_is_false_not_an_exception(self):
        with mock.patch.object(accelprobe, "_qmp_greeting_seen",
                               side_effect=OSError("no qemu")):
            self.assertFalse(accelprobe.probe("qemu-system-x86_64", "whpx"))


class Resolution(unittest.TestCase):
    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_a_working_hypervisor_is_kept(self):
        with mock.patch.object(accelprobe, "probe", return_value=True):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel, "whpx,kernel-irqchip=off")
        self.assertFalse(r.degraded)

    def test_a_host_without_one_falls_back_to_tcg_instead_of_failing(self):
        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel.split(",")[0], "tcg")
        self.assertTrue(r.degraded)

    def test_the_fallback_says_how_to_make_it_fast_again(self):
        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off",
                                   platform="windows")
        # The advice has to be actionable: name the feature AND the switch.
        self.assertIn("HypervisorPlatform", r.advice)
        self.assertIn("BIOS", r.advice)

    def test_every_platform_says_the_slow_boot_is_a_one_off(self):
        """Not consolation -- a fact, and one the code now has to keep. An
        emulated cold boot is minutes; the warm cache turns every launch after
        it into a restore, and TCG migrates where WHPX does not. Pinned here
        because the promise is only true while `_warm_cache_allowed` asks for
        the EFFECTIVE accelerator (see TheWarmCacheFollowsTheREALAccelerator);
        if that regresses, this message becomes a lie."""
        for platform in ("windows", "macos", "linux"):
            advice = accelprobe._advice_for(platform, "whpx")
            self.assertIn("cached", advice, platform)

    def test_linux_and_macos_get_their_own_advice(self):
        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            lin = accelprobe.resolve("qemu-system-x86_64", None,
                                     default="kvm", platform="linux")
            accelprobe.clear_cache()
            mac = accelprobe.resolve("qemu-system-aarch64", None,
                                     default="hvf", platform="macos")
        self.assertIn("/dev/kvm", lin.advice)
        self.assertIn("hvf", mac.advice.lower())

    def test_an_explicit_request_that_works_is_honoured_verbatim(self):
        with mock.patch.object(accelprobe, "probe", return_value=True):
            r = accelprobe.resolve("qemu-system-x86_64", "tcg",
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel, "tcg")
        self.assertFalse(r.degraded)
        self.assertTrue(r.requested)

    def test_an_explicit_request_that_does_not_work_still_boots_but_says_so(self):
        """--accel is a preference, not a suicide pact: the user asked for a
        machine, and a machine that boots slowly beats one that does not boot."""
        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            r = accelprobe.resolve("qemu-system-x86_64", "whpx",
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel.split(",")[0], "tcg")
        self.assertTrue(r.degraded)
        self.assertIn("whpx", r.note)

    def test_tcg_is_told_to_use_the_host_s_cores_when_it_can_be(self):
        """Single-threaded TCG on a weak PC is the difference between a slow
        boot and one nobody waits for."""
        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertIn("thread=multi", r.accel)

    def test_the_fallback_is_PROVEN_not_assumed(self):
        """THE BUG THIS CLASS OF TEST EXISTS FOR, found live 2026-08-21.

        TCG used to be returned unproven, on the reasoning that it is always
        compiled in and probing it would only slow down the path that is
        already slow. But x86 folds the accelerator into the MACHINE string
        (`-machine q35,accel=tcg,thread=multi`), where every item is a *machine*
        property -- and `thread` is an accelerator property, so QEMU refused to
        start at all:

            qemu-system-x86_64: Property 'pc-q35-11.1-machine.thread' not found

        The fallback nobody had tested would have failed outright on precisely
        the machines it exists to rescue. Nothing is exempt from the probe."""
        asked = []

        def records(_tool, accel, **_k):
            asked.append(accel)
            return False

        with mock.patch.object(accelprobe, "probe", side_effect=records):
            accelprobe.resolve("qemu-system-x86_64", "tcg,thread=multi",
                               default="whpx,kernel-irqchip=off")
        self.assertIn("tcg,thread=multi", asked,
                      "an explicit tcg request was returned without proving it")

    def test_a_rejected_multithreaded_tcg_falls_back_to_plain_tcg(self):
        def plain_only(_tool, accel, **_k):
            return accel == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=plain_only):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel, "tcg")
        self.assertTrue(r.degraded)

    def test_the_note_names_what_was_actually_chosen(self):
        def plain_only(_tool, accel, **_k):
            return accel == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=plain_only):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertIn("(tcg)", r.note)


class NoQemuIsUnknownNotBroken(unittest.TestCase):
    """"I could not ask" and "the answer is no" are different, and saying the
    second when you mean the first is how a first-boot machine gets told its
    virtualization is broken.

    Caught by running the FROZEN build, which is the whole reason that check
    exists: a build directory has no QEMU beside it until the installer puts
    one there, and `doctor` on it reported

        accel_hardware: True
        accel_note:     'neither whpx nor tcg initialises'

    -- contradictory, and the second half is a lie about the user's CPU. The
    app already models this correctly on its own side
    (bootstrap._WHPX_NO_QEMU_HINT, "virtualization has not been checked yet --
    QEMU is still being installed"); the engine did not.
    """

    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_a_missing_qemu_is_unknown(self):
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False):
            v = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertTrue(v.unknown)
        self.assertFalse(v.degraded)

    def test_it_does_not_claim_the_accelerators_are_broken(self):
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False):
            v = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertNotIn("initialises", v.note)
        self.assertIn("QEMU", v.note)

    def test_it_offers_no_bios_advice_for_a_question_it_never_asked(self):
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False):
            v = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(v.advice, "")

    def test_it_does_not_waste_a_probe(self):
        called = []
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False), \
                mock.patch.object(accelprobe, "probe",
                                  side_effect=lambda *a, **k: called.append(1)):
            accelprobe.resolve("qemu-system-x86_64", None, default="whpx")
        self.assertEqual(called, [])

    def test_the_verdict_is_not_cached_so_the_next_call_can_answer(self):
        """QEMU is ABOUT to be installed -- caching "unknown" would keep the
        deployment reporting unknown for the life of the process."""
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False):
            accelprobe.resolve("qemu-system-x86_64", None, default="whpx")
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=True), \
                mock.patch.object(accelprobe, "probe", return_value=True):
            v = accelprobe.resolve("qemu-system-x86_64", None, default="whpx")
        self.assertFalse(v.unknown)

    def test_doctor_reports_unknown_rather_than_hardware_true(self):
        from omnidroid import engine
        with mock.patch.object(accelprobe, "qemu_runnable", return_value=False):
            rep = engine._accel_readiness()
        self.assertIsNone(rep["accel_hardware"],
                          "a deployment with no QEMU claimed working hardware")


class ProbeIsPaidOnce(unittest.TestCase):
    """It sits in front of every spawn, and a pool filling ten slots must not
    pay it ten times."""

    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_the_verdict_is_cached_per_tool(self):
        calls = []

        def counting(_tool, accel, **_k):
            calls.append(accel)
            return True

        with mock.patch.object(accelprobe, "probe", side_effect=counting):
            for _ in range(5):
                accelprobe.resolve("qemu-system-x86_64", None, default="whpx")
        self.assertEqual(len(calls), 1)

    def test_a_different_tool_is_probed_separately(self):
        calls = []

        def counting(tool, accel, **_k):
            calls.append(tool)
            return True

        with mock.patch.object(accelprobe, "probe", side_effect=counting):
            accelprobe.resolve("qemu-system-x86_64", None, default="whpx")
            accelprobe.resolve("qemu-system-aarch64", None, default="whpx")
        self.assertEqual(len(calls), 2)


class NothingHereMayBreakABoot(unittest.TestCase):
    """The probe is an optimisation of the ERROR MESSAGE, not a precondition.
    If it cannot run at all, the caller must get the platform default back and
    boot exactly as it did before this module existed."""

    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_a_probe_that_explodes_leaves_the_default_in_place(self):
        with mock.patch.object(accelprobe, "probe",
                               side_effect=RuntimeError("boom")):
            r = accelprobe.resolve("qemu-system-x86_64", None,
                                   default="whpx,kernel-irqchip=off")
        self.assertEqual(r.accel, "whpx,kernel-irqchip=off")
        self.assertFalse(r.degraded)


class TheEngineUsesIt(unittest.TestCase):
    """Wiring, not mechanics: a resolved accelerator has to reach the argv."""

    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_default_accel_is_still_the_pure_platform_answer(self):
        # Kept probe-free on purpose: it is part of the warm-cache key and is
        # called from contexts with no QEMU to ask.
        from omnidroid import qemu_proc
        self.assertIn(qemu_proc.default_accel().split(",")[0],
                      ("whpx", "hvf", "kvm"))

    def test_effective_accel_prefers_the_proven_one(self):
        from omnidroid import qemu_proc

        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            got = qemu_proc.effective_accel("qemu-system-x86_64", None)
        self.assertEqual(got.split(",")[0], "tcg")


class TheWarmCacheFollowsTheREALAccelerator(unittest.TestCase):
    """The fast path has to reach the hosts that need it most.

    WHPX cannot migrate a VM (migfile.NON_MIGRATABLE_ACCELS), so the warm cache
    is refused on Windows and every launch cold-boots. TCG **can** migrate --
    and a host that has fallen back to TCG is precisely the host whose cold
    boots are unbearable, so it is the one with most to gain from a cache that
    replaces the boot entirely.

    The gate asked `default_accel()`, i.e. the platform's PREFERENCE, so on such
    a host it answered "whpx" and refused a cache that would in fact have
    worked. It has to ask what this machine is actually running.
    """

    def setUp(self):
        accelprobe.clear_cache()
        self.addCleanup(accelprobe.clear_cache)

    def test_whpx_still_refuses_the_cache(self):
        from omnidroid import engine
        self.assertFalse(
            engine._warm_cache_allowed(False, set(), "k",
                                       accel="whpx,kernel-irqchip=off"))

    def test_a_tcg_host_is_allowed_the_cache(self):
        from omnidroid import engine
        self.assertTrue(
            engine._warm_cache_allowed(False, set(), "k",
                                       accel="tcg,thread=multi"))

    def test_the_gate_resolves_rather_than_assuming_the_platform_default(self):
        """accel=None must mean "ask this machine", not "assume the platform"."""
        from omnidroid import engine

        def only_tcg(_tool, accel, **_k):
            return accel.split(",")[0] == "tcg"

        with mock.patch.object(accelprobe, "probe", side_effect=only_tcg):
            allowed = engine._warm_cache_allowed(False, set(), "k", accel=None)
        self.assertTrue(allowed,
                        "a host that has fallen back to TCG was refused the "
                        "warm cache because the gate asked the platform "
                        "instead of the machine")


if __name__ == "__main__":
    unittest.main()
