#!/usr/bin/env python3
"""Offline tests for the Roblox session layer (`omni play` / `omni session`).

Everything here is a pure function — no VM, no adb, no network — so the join URL
format, the token-redaction rule and the kiosk reply parsing are all pinned
without booting anything.

    python3 tests/test_session.py     (or: pytest tests/)
"""
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock
from urllib.parse import parse_qs, urlparse

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine as omni  # noqa: E402
from omnidroid import accounts as ck  # noqa: E402

TOKEN = "_|WARNING:-DO-NOT-SHARE-THIS.--" + ("A" * 200) + "ZZbEnD"


class DeepLink(unittest.TestCase):
    """The join URL must match what com.roblox.client.ActivityProtocolLaunch
    actually parses (verified against 2.726.1142)."""

    def test_place_only(self):
        u = omni.roblox_deeplink({"place_id": 1818})
        self.assertEqual(u, "roblox://experiences/start?placeId=1818")

    def test_no_place_means_no_link(self):
        self.assertIsNone(omni.roblox_deeplink({}))
        self.assertIsNone(omni.roblox_deeplink({"place_id": 0}))

    def test_optional_params_only_when_set(self):
        """The client reads a present-but-empty param as blank, not absent, so
        an unset field must not appear in the URL at all."""
        u = omni.roblox_deeplink({"place_id": 1818, "game_instance_id": "",
                                  "launch_data": None})
        self.assertNotIn("gameInstanceId", u)
        self.assertNotIn("launchData", u)

    def test_specific_server_and_launch_data(self):
        u = omni.roblox_deeplink({
            "place_id": 606849621,
            "game_instance_id": "8f2c-jobid",
            "launch_data": '{"roomId": 2}',
        })
        q = parse_qs(urlparse(u).query)
        self.assertEqual(q["placeId"], ["606849621"])
        self.assertEqual(q["gameInstanceId"], ["8f2c-jobid"])
        self.assertEqual(q["launchData"], ['{"roomId": 2}'])

    def test_launch_data_is_url_encoded(self):
        """launchData is arbitrary JSON; unencoded '&' would forge a param."""
        u = omni.roblox_deeplink({"place_id": 1, "launch_data": "a&placeId=999"})
        q = parse_qs(urlparse(u).query)
        self.assertEqual(q["placeId"], ["1"])
        self.assertEqual(q["launchData"], ["a&placeId=999"])

    def test_private_server_codes(self):
        u = omni.roblox_deeplink({"place_id": 7, "access_code": "abc",
                                  "link_code": "xyz"})
        q = parse_qs(urlparse(u).query)
        self.assertEqual(q["accessCode"], ["abc"])
        self.assertEqual(q["linkCode"], ["xyz"])


class TokenHandling(unittest.TestCase):
    """A .ROBLOSECURITY cookie is full account access. It must never be
    printable, loggable, or JSON-returnable."""

    def test_redaction_never_contains_the_token(self):
        r = omni.redact_token(TOKEN)
        self.assertNotIn(TOKEN, r)
        self.assertNotIn(TOKEN[:32], r)
        self.assertIn(str(len(TOKEN)), r)

    def test_redaction_of_absent_token(self):
        self.assertIsNone(omni.redact_token(None))
        self.assertIsNone(omni.redact_token(""))

    def test_public_session_strips_the_token(self):
        sess = {"token": TOKEN, "place_id": 1818, "user_id": 42}
        pub = omni.public_session(sess)
        self.assertNotIn(TOKEN, repr(pub))
        self.assertTrue(pub["has_token"])
        self.assertEqual(pub["place_id"], 1818)

    def test_public_session_without_token(self):
        pub = omni.public_session({"place_id": 1})
        self.assertFalse(pub["has_token"])
        self.assertIsNone(pub["token"])


class KioskReply(unittest.TestCase):
    """`am broadcast` prints the kiosk's ordered-broadcast result as data="...";
    that reply is how the host learns what happened IN the guest."""

    def test_parses_a_join(self):
        out = ('Broadcasting: Intent { act=com.omni.kiosk.SET_SESSION }\n'
               'Broadcast completed: result=-1, '
               'data="{\\"ok\\":true,\\"place_id\\":1818,\\"launched\\":true}"')
        r = omni._parse_broadcast_result(out)
        self.assertTrue(r["ok"])
        self.assertTrue(r["launched"])
        self.assertEqual(r["place_id"], 1818)

    def test_parses_an_error(self):
        out = ('Broadcast completed: result=-1, '
               'data="{\\"ok\\":false,\\"error\\":\\"roblox_not_installed\\"}"')
        r = omni._parse_broadcast_result(out)
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "roblox_not_installed")

    def test_no_reply_is_none_not_a_crash(self):
        """A receiver that never ran prints no data= at all."""
        self.assertIsNone(omni._parse_broadcast_result(
            "Broadcast completed: result=0"))
        self.assertIsNone(omni._parse_broadcast_result(""))


class PlaceValidation(unittest.TestCase):

    def test_accepts_numeric(self):
        self.assertEqual(omni._validate_place_id("606849621"), 606849621)
        self.assertEqual(omni._validate_place_id(1818), 1818)

    def test_rejects_junk(self):
        # fail() is contract-shaped: it exits rather than returning a sentinel.
        for bad in ("abc", "", None, "12x", "-5", "0"):
            with self.assertRaises(SystemExit, msg=f"accepted {bad!r}"):
                omni._validate_place_id(bad)

    def test_rejects_a_url_instead_of_an_id(self):
        """A pasted game URL is the likeliest mistake; it must not become a
        silently-wrong placeId."""
        with self.assertRaises(SystemExit):
            omni._validate_place_id(
                "https://www.roblox.com/games/606849621/Jailbreak")


class StoreSession(unittest.TestCase):
    """The session (token + place) now comes from the central account store,
    not a per-account session.json."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-test-store-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        patches = [
            mock.patch.object(omni, "REPO", self.tmp),
            mock.patch.dict(os.environ, {"OMNI_DATA_DIR": str(self.tmp)}),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_returns_stored_cookie_and_place(self):
        ck.save_account(str(self.tmp), "someuser", "cookie-abc")
        ck.set_fields(str(self.tmp), "someuser", place_id=606849621)
        sess = omni.store_session("someuser")
        self.assertEqual(sess["token"], "cookie-abc")
        self.assertEqual(sess["place_id"], 606849621)

    def test_unknown_account_has_no_token_or_place(self):
        sess = omni.store_session("nobody")
        self.assertIsNone(sess["token"])
        self.assertIsNone(sess["place_id"])

    def test_place_override_wins_over_stored_place(self):
        ck.save_account(str(self.tmp), "someuser", "cookie-abc")
        ck.set_fields(str(self.tmp), "someuser", place_id=111)
        sess = omni.store_session("someuser", place_override=222)
        self.assertEqual(sess["place_id"], 222)

    def test_transient_args_are_not_persisted(self):
        ck.save_account(str(self.tmp), "someuser", "cookie-abc")
        ck.set_fields(str(self.tmp), "someuser", place_id=111)
        args = SimpleNamespace(job="jobid-1", user_id=42, access_code=None,
                               link_code=None, launch_data=None)
        sess = omni.store_session("someuser", args)
        self.assertEqual(sess["game_instance_id"], "jobid-1")
        self.assertEqual(sess["user_id"], 42)
        # Re-reading the store (no args) must not see the transient job id.
        rec = ck.get_account(str(self.tmp), "someuser")
        self.assertNotIn("game_instance_id", rec)


class LoginTokenFlag(unittest.TestCase):
    """`omni login --token*` must be detected by PRESENCE, not truthiness — an
    empty `--token ""` / blank --token-file has to fail fast rather than
    silently falling back to the interactive browser flow (a headless,
    few-second call turning into an unattended 5-minute wait for a visible
    sign-in window). Regression coverage for that exact bug."""

    def test_no_flags_not_requested(self):
        args = SimpleNamespace(token=None, token_file=None, token_stdin=False)
        self.assertFalse(omni._token_flag_given(args))

    def test_empty_token_string_is_still_requested(self):
        args = SimpleNamespace(token="", token_file=None, token_stdin=False)
        self.assertTrue(omni._token_flag_given(args))

    def test_token_file_flag_is_requested_even_if_blank(self):
        args = SimpleNamespace(token=None, token_file="/tmp/whatever",
                               token_stdin=False)
        self.assertTrue(omni._token_flag_given(args))

    def test_token_stdin_flag_is_requested(self):
        args = SimpleNamespace(token=None, token_file=None, token_stdin=True)
        self.assertTrue(omni._token_flag_given(args))

    def test_real_token_is_requested(self):
        args = SimpleNamespace(token="abc123", token_file=None, token_stdin=False)
        self.assertTrue(omni._token_flag_given(args))


class PlayGatesOnLogin(unittest.TestCase):
    """`omni start <name>` must never create an instance (overlay + /data +
    QEMU disks) for a name with no saved cookie and no override. Regression
    coverage for the bug where ensure_instance() ran BEFORE the token check,
    so a brand-new or misspelled name (e.g. a literal 'omniagent') left a
    real instance directory on disk even though the launch always failed
    with no_token — "the only way to create a profile is to log in" was not
    actually enforced."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-test-repo-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        accounts_dir = self.tmp / "accounts"
        patches = [
            mock.patch.object(omni, "REPO", self.tmp),
            mock.patch.object(omni, "ACCOUNTS_DIR", accounts_dir),
            # The cookie store now routes through config.data_dir() (see
            # omnidroid.config / engine._store_root); redirect it alongside
            # REPO so a saved account is found where this test saves it.
            mock.patch.dict(os.environ, {"OMNI_DATA_DIR": str(self.tmp)}),
            mock.patch.object(omni, "ensure_qemu", lambda: None),
            mock.patch.object(omni, "load_config", lambda: {}),
            mock.patch.object(omni, "running_pid", lambda name: None),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def _args(self, **over):
        base = dict(name="brand_new_name", place="606849621", token=None,
                    token_file=None, token_stdin=False, job=None,
                    access_code=None, link_code=None, launch_data=None,
                    user_id=None, no_token=False, dev=False, window=False,
                    no_window=True, mode=None, mem=None, accel=None,
                    timeout=None, json=True)
        base.update(over)
        return SimpleNamespace(**base)

    def test_no_token_never_creates_an_instance(self):
        with mock.patch.object(omni, "build_acct") as build_mock:
            with self.assertRaises(SystemExit):
                omni.cmd_start(self._args())
            build_mock.assert_not_called()
        self.assertFalse((self.tmp / "accounts" / "brand_new_name").exists())

    def test_explicit_no_token_still_reaches_build_acct(self):
        """--no-token is a deliberate, explicit escape hatch (land on
        Roblox's own login screen) — the one case allowed to proceed without
        a cookie, so it must still reach build_acct()."""
        stub_acct = {"name": "brand_new_name", "adb_port": 1, "vnc_port": 1,
                    "base": "dev", "game_package": omni.ROBLOX_PACKAGE}
        with mock.patch.object(omni, "build_acct",
                              return_value=stub_acct) as build_mock, \
             mock.patch.object(omni, "_ensure_booted", return_value=(False, True)), \
             mock.patch.object(omni, "acct_arch", return_value="arm"), \
             mock.patch.object(omni, "acct_is_dev", return_value=True):
            with self.assertRaises(SystemExit):
                omni.cmd_start(self._args(no_token=True))
        build_mock.assert_called_once()

    def test_saved_account_still_reaches_build_acct(self):
        """A name that IS a saved Roblox account (has a cookie) must keep
        working exactly as before — this only gates a name with NO cookie."""
        ck.save_account(str(self.tmp), "realuser", "sometoken")
        stub_acct = {"name": "realuser", "adb_port": 1, "vnc_port": 1,
                    "base": "dev", "game_package": omni.ROBLOX_PACKAGE}
        with mock.patch.object(omni, "build_acct",
                              return_value=stub_acct) as build_mock, \
             mock.patch.object(omni, "_ensure_booted", return_value=(False, True)), \
             mock.patch.object(omni, "acct_arch", return_value="arm"), \
             mock.patch.object(omni, "acct_is_dev", return_value=True):
            with self.assertRaises(SystemExit):
                omni.cmd_start(self._args(name="realuser"))
        build_mock.assert_called_once()

    def test_saved_account_with_no_place_still_reaches_build_acct(self):
        """A place is now OPTIONAL: no place means a HOME boot, not a
        rejection. A saved account with no place must still reach
        build_acct() (it used to fail with no_place before any instance
        could be created)."""
        ck.save_account(str(self.tmp), "realuser", "sometoken")
        stub_acct = {"name": "realuser", "adb_port": 1, "vnc_port": 1,
                    "base": "dev", "game_package": omni.ROBLOX_PACKAGE}
        with mock.patch.object(omni, "build_acct",
                              return_value=stub_acct) as build_mock, \
             mock.patch.object(omni, "_ensure_booted", return_value=(False, True)), \
             mock.patch.object(omni, "acct_arch", return_value="arm"), \
             mock.patch.object(omni, "acct_is_dev", return_value=True):
            with self.assertRaises(SystemExit):
                omni.cmd_start(self._args(name="realuser", place=None))
        build_mock.assert_called_once()


class StartHomeVsJoin(unittest.TestCase):
    """`omni start` with no --place boots to HOME (play=False, cookie
    delivered, nothing joined); with --place it JOINS (play=True)."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-test-repo-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        patches = [
            mock.patch.object(omni, "REPO", self.tmp),
            mock.patch.object(omni, "ACCOUNTS_DIR", self.tmp / "accounts"),
            mock.patch.dict(os.environ, {"OMNI_DATA_DIR": str(self.tmp)}),
            mock.patch.object(omni, "ensure_qemu", lambda: None),
            mock.patch.object(omni, "load_config", lambda: {}),
            mock.patch.object(omni, "running_pid", lambda name: None),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        ck.save_account(str(self.tmp), "realuser", "sometoken")

    def _args(self, **over):
        base = dict(name="realuser", place=None, token=None, token_file=None,
                    token_stdin=False, job=None, access_code=None,
                    link_code=None, launch_data=None, user_id=None,
                    no_token=False, dev=False, window=False, no_window=True,
                    mode=None, mem=None, accel=None, timeout=None, json=True)
        base.update(over)
        return SimpleNamespace(**base)

    def _run(self, **over):
        stub_acct = {"name": "realuser", "adb_port": 1, "vnc_port": 1,
                    "base": "dev", "game_package": omni.ROBLOX_PACKAGE}
        with mock.patch.object(omni, "build_acct", return_value=stub_acct), \
             mock.patch.object(omni, "_ensure_booted", return_value=(True, False)), \
             mock.patch.object(omni, "acct_arch", return_value="arm"), \
             mock.patch.object(omni, "acct_is_dev", return_value=True), \
             mock.patch.object(omni, "deliver_session",
                              return_value={"delivered": True}) as deliver_mock, \
             mock.patch.object(omni, "_spawn_builtin_viewer"):
            omni.cmd_start(self._args(**over))
        return deliver_mock

    def test_no_place_delivers_home_not_join(self):
        deliver_mock = self._run(place=None)
        deliver_mock.assert_called_once()
        _, kwargs = deliver_mock.call_args
        # play is passed positionally or as kwarg depending on call site;
        # cmd_start calls deliver_session(acct, label, sess, play=is_join).
        self.assertFalse(deliver_mock.call_args.kwargs.get("play", True))

    def test_place_delivers_join(self):
        deliver_mock = self._run(place="606849621")
        deliver_mock.assert_called_once()
        self.assertTrue(deliver_mock.call_args.kwargs.get("play", False))


if __name__ == "__main__":
    unittest.main(verbosity=2)
