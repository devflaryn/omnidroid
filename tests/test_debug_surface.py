#!/usr/bin/env python3
"""The debugging surface both AIs (Claude Code and omni-agent) drive.

    python3 tests/test_debug_surface.py

The requirement is "any AI should be able to debug, screenshot, launch
accounts and join places, test new APKs, create/delete offsets, run su
commands and use frida-server". Most of that already existed as commands; the
gaps were ROOT and FRIDA, which every caller had been hand-rolling — and
getting wrong in the same three ways every time:

  1. Magisk's su is not on $PATH (/debug_ramdisk/su, not /system/bin/su);
  2. MagiskSU permutes argv, so `su 0 id -u` is read as an su OPTION;
  3. `adb shell` JOINS its argv and re-parses it in the guest shell, so an
     unquoted `a; b` runs a fragment of itself and still reports success.

All three are silent. `omnidroid su` gets them right once so nothing else has
to; these tests are what stop a refactor from quietly undoing that.
"""
import os
import shlex
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _args(**kw):
    kw.setdefault("json", False)
    return type("A", (), kw)()


ACCT = {"name": "u1", "base": "arm", "adb_port": 16001, "qmp_port": 17001,
        "vnc_port": 18001}


class EveryCapabilityIsAdvertised(unittest.TestCase):
    """A client (omni-agent, omni-executor) capability-checks before calling."""

    def test_the_commands_list_carries_the_whole_surface(self):
        cmds = set(omni.registered_commands())
        for c in ("start", "stop", "list", "screenshot", "logcat", "capture",
                  "adb", "su", "frida", "debug-info", "offset", "install",
                  "test-apk", "login", "accounts", "session", "view",
                  "build-devkit", "root-base"):
            self.assertIn(c, cmds, c)

    def test_the_commands_list_is_derived_from_the_parser(self):
        parser = omni.build_parser()
        choices = next(a.choices for a in parser._actions
                       if getattr(a, "dest", None) == "cmd" and a.choices)
        self.assertEqual(sorted(choices), omni.registered_commands())


class SuQuoting(unittest.TestCase):
    def _run_su(self, rest):
        seen = {}

        def fake_adb(acct, *a, **kw):
            seen["argv"] = a
            return mock.Mock(stdout="0\n", stderr="", returncode=0)

        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su",
                               return_value="/debug_ramdisk/su"), \
             mock.patch.object(omni, "adb", fake_adb):
            with self.assertRaises(SystemExit) as e:
                omni.cmd_su(_args(name="u1", rest=rest, timeout=30))
        return seen["argv"], e.exception.code

    def test_it_goes_through_sh_c_not_a_bare_su(self):
        # `su 0 id -u` would be misread as an su option and exit 2.
        argv, _ = self._run_su(["id", "-u"])
        self.assertEqual(argv[0], "shell")
        self.assertIn("/debug_ramdisk/su 0 sh -c ", argv[1])

    def test_the_whole_script_is_quoted_as_ONE_adb_argument(self):
        # adb re-parses a joined argv; a multi-command script that is not
        # quoted runs only its first fragment and still reports success.
        argv, _ = self._run_su(["pm list packages; echo done"])
        self.assertEqual(len(argv), 2)
        quoted = argv[1].split("sh -c ", 1)[1]
        self.assertEqual(shlex.split(quoted), ["pm list packages; echo done"])

    def test_it_exits_with_the_guest_commands_own_status(self):
        _argv, code = self._run_su(["id", "-u"])
        self.assertEqual(code, 0)

    def test_an_unrooted_instance_fails_loudly_instead_of_downgrading(self):
        # Running as uid shell instead would produce WRONG output that looks
        # right — the worst outcome for a debugging session.
        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su", return_value=None), \
             mock.patch.object(omni, "adb") as adb:
            with self.assertRaises(SystemExit):
                omni.cmd_su(_args(name="u1", rest=["id"], timeout=30))
        self.assertFalse(adb.called)

    def test_an_empty_command_is_refused(self):
        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su", return_value="su"), \
             mock.patch.object(omni, "adb"):
            with self.assertRaises(SystemExit):
                omni.cmd_su(_args(name="u1", rest=[], timeout=30))


class Frida(unittest.TestCase):
    def test_a_non_debug_boot_says_so_rather_than_failing_obscurely(self):
        # "no devkit disk" and "devkit but unrooted" need DIFFERENT fixes;
        # the generic message that conflated them sent people to the wrong one.
        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su", return_value="su"), \
             mock.patch.object(omni, "adb",
                               return_value=mock.Mock(stdout="No such file",
                                                      stderr="")):
            with self.assertRaises(SystemExit) as e:
                omni.cmd_frida(_args(name="u1", status=False, stop=False,
                                     port=None))
        self.assertEqual(e.exception.code, 1)

    def test_an_unrooted_instance_is_a_different_error(self):
        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su", return_value=None), \
             mock.patch.object(omni, "adb") as adb:
            with self.assertRaises(SystemExit):
                omni.cmd_frida(_args(name="u1", status=False, stop=False,
                                     port=None))
        self.assertFalse(adb.called)

    def test_the_default_guest_port_is_not_the_well_known_one(self):
        # The devkit deliberately hides frida-server off 27042 so a naive
        # port scan by the app under test misses it.
        self.assertNotEqual(omni.DEFAULT_FRIDA_PORT, 27042)

    def test_status_probes_the_port_not_the_process_name(self):
        # The server runs under a randomized name, so a name-based check
        # would report "not running" every time.
        seen = {}

        def fake_adb(acct, *a, **kw):
            seen["argv"] = a
            return mock.Mock(stdout="tcp 0 0 127.0.0.1:27142 LISTEN",
                             stderr="")

        with mock.patch.object(omni, "adb", fake_adb):
            running, _ = omni._frida_status(dict(ACCT), "su", 27142)
        self.assertTrue(running)
        self.assertIn("27142", seen["argv"][1])
        self.assertNotIn("frida-server", seen["argv"][1])


class DebugInfo(unittest.TestCase):
    def _info(self, su, has_vdc):
        out = {}

        def fake_adb(acct, *a, **kw):
            if a[:2] == ("shell", "ls"):
                return mock.Mock(stdout="/dev/block/vdc" if has_vdc
                                 else "No such file", stderr="")
            return mock.Mock(stdout="", stderr="")

        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "running_pid", return_value=4242), \
             mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "resolve_su", return_value=su), \
             mock.patch.object(omni, "_foreground", return_value="com.x/.A"), \
             mock.patch.object(omni, "_frida_status",
                               return_value=(False, "")), \
             mock.patch.object(omni, "adb", fake_adb), \
             mock.patch.object(omni, "emit_json",
                               side_effect=lambda d: out.update(d)):
            omni.cmd_debug_info(_args(name="u1", json=True))
        return out

    def test_it_reports_what_is_true_not_what_was_requested(self):
        rep = self._info(su=None, has_vdc=False)
        self.assertFalse(rep["root"]["available"])
        self.assertFalse(rep["devkit"]["attached"])
        self.assertFalse(rep["can"]["frida"])
        self.assertFalse(rep["can"]["run_su"])
        # ...but the things that never needed root are still true.
        self.assertTrue(rep["can"]["screenshot"])
        self.assertTrue(rep["can"]["logcat"])

    def test_each_missing_capability_carries_its_own_fix(self):
        rep = self._info(su=None, has_vdc=False)
        self.assertIn("root-base", rep["root"]["fix"])
        self.assertIn("--debug", rep["devkit"]["fix"])

    def test_root_plus_devkit_unlocks_frida(self):
        rep = self._info(su="/debug_ramdisk/su", has_vdc=True)
        self.assertTrue(rep["can"]["frida"])
        self.assertTrue(rep["can"]["hide_root"])
        self.assertIsNone(rep["root"]["fix"])

    def test_a_stopped_instance_is_refused_cleanly(self):
        with mock.patch.object(omni, "load_account", return_value=dict(ACCT)), \
             mock.patch.object(omni, "running_pid", return_value=None):
            with self.assertRaises(SystemExit):
                omni.cmd_debug_info(_args(name="u1"))


class TheVersionHandshake(unittest.TestCase):
    def _rep(self):
        out = {}
        with mock.patch.object(omni, "emit_json",
                               side_effect=lambda d: out.update(d)):
            omni.cmd_version(_args(json=True))
        return out

    def test_it_advertises_offsets_so_a_client_can_offer_a_version_picker(self):
        caps = self._rep()["capabilities"]
        self.assertTrue(caps["offsets"]["supported"])
        self.assertTrue(caps["offsets"]["clean_base"])
        self.assertFalse(caps["offsets"]["per_account"])

    def test_it_advertises_the_debug_surface(self):
        caps = self._rep()["capabilities"]["debug"]
        for k in ("su", "frida", "screenshot", "logcat", "apk_install",
                  "devkit_boot", "info"):
            self.assertTrue(caps[k], k)

    def test_it_reports_which_versions_are_baked_and_which_is_default(self):
        rep = self._rep()
        self.assertIn("offsets", rep)
        for _tag, o in rep["offsets"].items():
            self.assertIn("default", o)
            self.assertIsInstance(o["available"], list)


if __name__ == "__main__":
    unittest.main()
