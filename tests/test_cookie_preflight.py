#!/usr/bin/env python3
"""Host-side preflight: is the saved .ROBLOSECURITY still a live session?

WHY THIS EXISTS (root-caused on-device 2026-07-20): the OmniBootstrap logcat
marker `session cookie installed (N chars)` proves the bootstrapped APK
INJECTED the cookie -- it says NOTHING about whether Roblox ACCEPTED it. An
expired cookie injects perfectly, lands on the Sign In page, and still emits
the marker, so `_await_bootstrap_login` returns True and `start` reports
success while the user stares at a login screen.

Tri-state is the whole point:
  True  -> authenticated, boot away
  False -> Roblox definitively rejected it (401) -> abort loudly, no wasted boot
  None  -> the CHECK ITSELF could not run (no network, no curl, timeout)
           -> callers MUST fail open, never block a boot on our own blindness.

    python3 tests/test_cookie_preflight.py     (or: pytest tests/)
"""
import contextlib
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import accounts as ck  # noqa: E402

AUTHED = json.dumps({"id": 1234567, "name": "admn1b12farm3",
                     "displayName": "admn1b12farm3"})
# Verbatim body Roblox returned for the dead cookie (2026-07-20).
UNAUTHED = json.dumps({"errors": [{"code": 9002, "subcode": 1,
                                   "message": "User is not authenticated"}]})


def _curl(stdout, returncode=0):
    """Stub of the curl invocation: body then the HTTP_CODE sentinel."""
    return SimpleNamespace(stdout=stdout, stderr="", returncode=returncode)


class ValidateRobloxCookie(unittest.TestCase):
    def test_live_session_is_ok_true_with_identity(self):
        with mock.patch.object(omni, "_curl_json",
                               return_value=(200, AUTHED)):
            res = omni.validate_roblox_cookie("x" * 1199)
        self.assertIs(res["ok"], True)
        self.assertEqual(res["user_id"], 1234567)
        self.assertEqual(res["username"], "admn1b12farm3")

    def test_401_is_a_definitive_rejection(self):
        with mock.patch.object(omni, "_curl_json",
                               return_value=(401, UNAUTHED)):
            res = omni.validate_roblox_cookie("x" * 1199)
        self.assertIs(res["ok"], False)
        self.assertEqual(res["error"], "cookie_invalid")

    def test_network_failure_is_unknown_not_rejection(self):
        # Fail OPEN: an offline host must still be able to boot.
        with mock.patch.object(omni, "_curl_json",
                               return_value=(None, "")):
            res = omni.validate_roblox_cookie("x" * 1199)
        self.assertIsNone(res["ok"])
        self.assertEqual(res["error"], "check_unavailable")

    def test_server_error_is_unknown_not_rejection(self):
        # A Roblox 5xx says nothing about OUR cookie.
        with mock.patch.object(omni, "_curl_json",
                               return_value=(503, "upstream boom")):
            res = omni.validate_roblox_cookie("x" * 1199)
        self.assertIsNone(res["ok"])

    def test_unparseable_200_is_unknown_not_success(self):
        # A captive portal / proxy returning HTML 200 must not read as authed.
        with mock.patch.object(omni, "_curl_json",
                               return_value=(200, "<html>login</html>")):
            res = omni.validate_roblox_cookie("x" * 1199)
        self.assertIsNone(res["ok"])

    def test_empty_cookie_is_rejected_without_a_network_call(self):
        with mock.patch.object(omni, "_curl_json") as cj:
            res = omni.validate_roblox_cookie("")
        self.assertIs(res["ok"], False)
        cj.assert_not_called()

    def test_cookie_is_never_echoed_in_the_result(self):
        # Results get printed and JSON-dumped; a session cookie must not ride along.
        secret = "SECRET" * 200
        with mock.patch.object(omni, "_curl_json",
                              return_value=(401, UNAUTHED)):
            res = omni.validate_roblox_cookie(secret)
        self.assertNotIn("SECRET", json.dumps(res))


class CurlJsonTransport(unittest.TestCase):
    """The transport shells out to curl: this host's Python cannot do TLS
    (CERTIFICATE_VERIFY_FAILED -- no system CA store), while curl works."""

    def test_sends_the_cookie_as_a_header_and_parses_the_status(self):
        seen = {}

        def fake_run(argv, **kw):
            seen["argv"] = argv
            return _curl("{}\nHTTP_CODE=200")

        with mock.patch.object(omni.subprocess, "run", fake_run):
            code, body = omni._curl_json(omni.ROBLOX_AUTH_URL, "tok123")
        self.assertEqual(code, 200)
        self.assertEqual(body.strip(), "{}")
        joined = " ".join(seen["argv"])
        self.assertIn(".ROBLOSECURITY=tok123", joined)
        self.assertIn(omni.ROBLOX_AUTH_URL, joined)

    def test_missing_curl_yields_unknown(self):
        with mock.patch.object(omni.subprocess, "run",
                               side_effect=FileNotFoundError("no curl")):
            code, body = omni._curl_json(omni.ROBLOX_AUTH_URL, "tok")
        self.assertIsNone(code)

    def test_timeout_yields_unknown(self):
        with mock.patch.object(omni.subprocess, "run",
                               side_effect=omni.subprocess.TimeoutExpired("curl", 5)):
            code, body = omni._curl_json(omni.ROBLOX_AUTH_URL, "tok")
        self.assertIsNone(code)


class PreflightGatesTheBoot(unittest.TestCase):
    """A dead cookie must abort BEFORE anything boots; an unreachable check
    must never block a boot."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-preflight-"))
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
                    mode=None, mem=None, accel=None, timeout=None, json=True,
                    apk=None, no_cookie_check=False)
        base.update(over)
        return SimpleNamespace(**base)

    def _run(self, validate_result, **over):
        build = mock.Mock(return_value={"name": "realuser", "adb_port": 1,
                                        "vnc_port": 1, "base": "arm"})
        booted = mock.Mock(return_value=(True, False))
        with mock.patch.object(omni, "validate_roblox_cookie",
                               return_value=validate_result) as vc, \
             mock.patch.object(omni, "build_acct", build), \
             mock.patch.object(omni, "_ensure_booted", booted), \
             mock.patch.object(omni, "acct_arch", return_value="arm"), \
             mock.patch.object(omni, "acct_is_dev", return_value=False), \
             mock.patch.object(omni, "roblox_deeplink", return_value=None), \
             mock.patch.object(omni, "public_session", return_value={}), \
             mock.patch.object(omni, "deliver_session",
                               return_value={"delivered": True,
                                             "kiosk": {"ok": True}}):
            with contextlib.suppress(SystemExit):
                omni.cmd_start(self._args(**over))
        return vc, build, booted

    def test_dead_cookie_aborts_before_any_boot(self):
        _vc, build, booted = self._run(
            {"ok": False, "error": "cookie_invalid", "detail": "expired"})
        build.assert_not_called()
        booted.assert_not_called()

    def test_unreachable_check_fails_open_and_boots(self):
        _vc, build, booted = self._run(
            {"ok": None, "error": "check_unavailable", "detail": "offline"})
        build.assert_called_once()
        booted.assert_called_once()

    def test_live_cookie_boots(self):
        _vc, build, booted = self._run(
            {"ok": True, "user_id": 1, "username": "realuser"})
        build.assert_called_once()
        booted.assert_called_once()

    def test_no_cookie_check_flag_skips_the_call_entirely(self):
        vc, build, _booted = self._run(
            {"ok": False, "error": "cookie_invalid", "detail": "expired"},
            no_cookie_check=True)
        vc.assert_not_called()
        build.assert_called_once()

    def test_no_token_boot_does_not_check(self):
        # --no-token deliberately lands on Roblox's own login screen; there is
        # no cookie to validate, so the preflight must not run (or reject).
        ck.save_account(str(self.tmp), "realuser", "")
        vc, _build, _booted = self._run(
            {"ok": False, "error": "cookie_invalid", "detail": "expired"},
            no_token=True)
        vc.assert_not_called()


if __name__ == "__main__":
    unittest.main(verbosity=2)
