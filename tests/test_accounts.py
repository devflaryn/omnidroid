#!/usr/bin/env python3
"""Offline tests for the account cookie store (`omni login` / `omni accounts`).

No browser, no network: the Selenium flow needs a human at a login page by
design, so what is testable here is the part that must never go wrong — the
single-file store keyed by username, its permissions, migration from the old
per-label files, and the rule that a cookie never leaks into output.

    python3 tests/test_cookies.py
"""
import json
import os
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import accounts as cookies  # noqa: E402

TOKEN = "_|WARNING:-DO-NOT-SHARE-THIS.--" + ("Q" * 300) + "tAiL99"


class Store(unittest.TestCase):

    def setUp(self):
        self.repo = tempfile.mkdtemp(prefix="omni-ck-")
        self.addCleanup(shutil.rmtree, self.repo, True)

    def test_saves_and_loads_by_username(self):
        cookies.save_account(self.repo, "someuser", TOKEN, 42)
        rec = cookies.get_account(self.repo, "someuser")
        self.assertEqual(rec["cookie"], TOKEN)
        self.assertEqual(rec["username"], "someuser")
        self.assertEqual(rec["user_id"], 42)

    def test_one_file_holds_all_accounts(self):
        cookies.save_account(self.repo, "userA", TOKEN, 1)
        cookies.save_account(self.repo, "userB", TOKEN + "b", 2)
        # exactly one store file, both accounts inside it
        self.assertTrue(cookies.accounts_path(self.repo).exists())
        data = json.loads(cookies.accounts_path(self.repo).read_text())
        self.assertEqual(sorted(data["accounts"]), ["userA", "userB"])

    def test_store_file_is_not_world_readable(self):
        cookies.save_account(self.repo, "u", TOKEN)
        mode = os.stat(cookies.accounts_path(self.repo)).st_mode & 0o777
        self.assertEqual(mode, 0o600, f"store is {oct(mode)}, want 0600")

    def test_listing_never_exposes_the_cookie(self):
        """`omni accounts` output is printed and JSON-returned; a cookie in it
        would end up in terminal scrollback, logs, and agent transcripts."""
        cookies.save_account(self.repo, "someuser", TOKEN, 42)
        blob = json.dumps(cookies.list_accounts(self.repo))
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn(TOKEN[:40], blob)
        accts = cookies.list_accounts(self.repo)
        self.assertTrue(accts[0]["has_cookie"])
        self.assertEqual(accts[0]["username"], "someuser")

    def test_missing_account_is_none_not_a_crash(self):
        self.assertIsNone(cookies.get_account(self.repo, "nope"))

    def test_upsert_overwrites_same_username(self):
        cookies.save_account(self.repo, "u", "old", 1)
        cookies.save_account(self.repo, "u", "new", 1)
        self.assertEqual(cookies.get_account(self.repo, "u")["cookie"], "new")

    def test_new_fields_default_to_none_on_first_save(self):
        cookies.save_account(self.repo, "u", TOKEN, 7)
        rec = cookies.get_account(self.repo, "u")
        for f in ("place_id", "base", "proxy", "group", "notes"):
            self.assertIn(f, rec)
            self.assertIsNone(rec[f])

    def test_new_fields_survive_relogin(self):
        cookies.save_account(self.repo, "u", "old", 1)
        # simulate A2.2 setting fields, then a routine cookie refresh
        data = json.loads(cookies.accounts_path(self.repo).read_text())
        data["accounts"]["u"]["place_id"] = 123
        data["accounts"]["u"]["base"] = "dev"
        data["accounts"]["u"]["group"] = "farm-a"
        cookies.accounts_path(self.repo).write_text(json.dumps(data))
        cookies.save_account(self.repo, "u", "new", 1)   # re-login
        rec = cookies.get_account(self.repo, "u")
        self.assertEqual(rec["cookie"], "new")           # cookie refreshed
        self.assertEqual(rec["place_id"], 123)           # field preserved
        self.assertEqual(rec["base"], "dev")
        self.assertEqual(rec["group"], "farm-a")

    def test_listing_exposes_place_base_group_but_not_proxy_notes(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        data = json.loads(cookies.accounts_path(self.repo).read_text())
        data["accounts"]["u"].update(
            {"place_id": 9, "base": "prod", "group": "g",
             "proxy": "http://secret:pw@host", "notes": "n"})
        cookies.accounts_path(self.repo).write_text(json.dumps(data))
        entry = cookies.list_accounts(self.repo)[0]
        self.assertEqual(entry["place_id"], 9)
        self.assertEqual(entry["base"], "prod")
        self.assertEqual(entry["group"], "g")
        self.assertNotIn("proxy", entry)
        self.assertNotIn("notes", entry)
        self.assertNotIn("cookie", entry)

    def test_set_fields_updates_existing_account(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        ok = cookies.set_fields(self.repo, "u",
                                place_id="4483381587", base="dev",
                                group="farm-a", notes="test")
        self.assertTrue(ok)
        rec = cookies.get_account(self.repo, "u")
        self.assertEqual(rec["place_id"], 4483381587)   # coerced to int
        self.assertEqual(rec["base"], "dev")
        self.assertEqual(rec["group"], "farm-a")
        self.assertEqual(rec["notes"], "test")
        self.assertEqual(rec["cookie"], TOKEN)           # cookie untouched

    def test_set_fields_returns_false_for_missing_account(self):
        self.assertFalse(cookies.set_fields(self.repo, "ghost", base="prod"))

    def test_set_fields_rejects_bad_place_id(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        with self.assertRaises(ValueError):
            cookies.set_fields(self.repo, "u", place_id="-5")
        with self.assertRaises(ValueError):
            cookies.set_fields(self.repo, "u", place_id="notanumber")

    def test_set_fields_rejects_bad_base_and_unknown_field(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        with self.assertRaises(ValueError):
            cookies.set_fields(self.repo, "u", base="staging")
        with self.assertRaises(ValueError):
            cookies.set_fields(self.repo, "u", cookie="hacked")

    def test_set_fields_clears_with_none(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        cookies.set_fields(self.repo, "u", place_id=7)
        cookies.set_fields(self.repo, "u", place_id=None)
        self.assertIsNone(cookies.get_account(self.repo, "u")["place_id"])


class CustomName(unittest.TestCase):
    """A friendly custom_name is a display-only label alongside the username —
    the username stays the account's real identity (and the instance name);
    custom_name must survive a routine cookie refresh, not get wiped by it."""

    def setUp(self):
        self.repo = tempfile.mkdtemp(prefix="omni-ck-")
        self.addCleanup(shutil.rmtree, self.repo, True)

    def test_set_custom_name_on_existing_account(self):
        cookies.save_account(self.repo, "erin7231", TOKEN, 1)
        ok = cookies.set_custom_name(self.repo, "erin7231", "Farm 3")
        self.assertTrue(ok)
        rec = cookies.get_account(self.repo, "erin7231")
        self.assertEqual(rec["custom_name"], "Farm 3")
        self.assertEqual(rec["username"], "erin7231")   # identity unchanged

    def test_set_custom_name_on_unknown_account_fails(self):
        self.assertFalse(cookies.set_custom_name(self.repo, "nope", "X"))

    def test_relogin_preserves_custom_name(self):
        """Re-running `omni login` (a cookie refresh) must not silently wipe a
        custom_name someone attached earlier."""
        cookies.save_account(self.repo, "erin7231", TOKEN, 1)
        cookies.set_custom_name(self.repo, "erin7231", "Farm 3")
        cookies.save_account(self.repo, "erin7231", TOKEN + "-refreshed", 1)
        rec = cookies.get_account(self.repo, "erin7231")
        self.assertEqual(rec["custom_name"], "Farm 3")
        self.assertEqual(rec["cookie"], TOKEN + "-refreshed")

    def test_empty_string_clears_custom_name(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        cookies.set_custom_name(self.repo, "u", "Label")
        cookies.set_custom_name(self.repo, "u", "")
        self.assertIsNone(cookies.get_account(self.repo, "u")["custom_name"])

    def test_custom_name_never_exposes_the_cookie(self):
        cookies.save_account(self.repo, "u", TOKEN, 1)
        cookies.set_custom_name(self.repo, "u", "Label")
        blob = json.dumps(cookies.list_accounts(self.repo))
        self.assertNotIn(TOKEN, blob)
        self.assertIn("Label", blob)
        self.assertEqual(len(cookies.list_accounts(self.repo)), 1)

    def test_remove(self):
        cookies.save_account(self.repo, "u", TOKEN)
        self.assertTrue(cookies.remove_account(self.repo, "u"))
        self.assertFalse(cookies.remove_account(self.repo, "u"))
        self.assertEqual(cookies.list_accounts(self.repo), [])

    def test_corrupt_store_is_not_fatal(self):
        cookies.accounts_path(self.repo).write_text("{not json")
        # a corrupt file must not crash reads; it degrades to empty + can be
        # overwritten by the next save
        self.assertEqual(cookies.list_accounts(self.repo), [])
        cookies.save_account(self.repo, "u", TOKEN)
        self.assertEqual([a["username"] for a in cookies.list_accounts(self.repo)],
                         ["u"])

    def test_migrates_legacy_per_label_files(self):
        """Old cookies/<label>.json folds into accounts.json keyed by username."""
        legacy = os.path.join(self.repo, "cookies")
        os.makedirs(legacy)
        with open(os.path.join(legacy, "main.json"), "w") as fh:
            json.dump({"label": "main", "username": "realname",
                       "user_id": 7, "cookie": TOKEN}, fh)
        # keyed by the stored username, not the label
        rec = cookies.get_account(self.repo, "realname")
        self.assertIsNotNone(rec)
        self.assertEqual(rec["cookie"], TOKEN)
        self.assertIsNone(cookies.get_account(self.repo, "main"))


class Whoami(unittest.TestCase):

    def test_bad_cookie_is_none_not_an_exception(self):
        """Offline/garbage input must degrade to (None, None). A raised
        exception here would abort `omni accounts --verify` on one dead cookie."""
        uid, uname = cookies.whoami("definitely-not-a-real-cookie", timeout=8)
        self.assertIsNone(uid)
        self.assertIsNone(uname)


class _FakeDriver:
    """Minimal Selenium WebDriver stand-in. `current_url` walks through
    `url_sequence` (repeating the last entry once exhausted), so a test can
    script "not landed yet" -> "landed" without a real browser."""

    def __init__(self, url_sequence):
        self._urls = list(url_sequence)
        self.cookies_added = []
        self.gets = []
        self.quit_called = False

    def get(self, url):
        self.gets.append(url)

    def add_cookie(self, cookie):
        self.cookies_added.append(cookie)

    @property
    def current_url(self):
        if len(self._urls) > 1:
            return self._urls.pop(0)
        return self._urls[0]

    def quit(self):
        self.quit_called = True


class CaptureLoginFromCookie(unittest.TestCase):
    """`omni login --token*`: adopt an already-obtained cookie via a HEADLESS
    browser instead of an interactive sign-in. No real Chrome here — `_driver`
    and `whoami` are mocked so this stays as offline as the rest of the file;
    the browser/network parts are exercised manually (see the CLI itself)."""

    def setUp(self):
        self.repo = tempfile.mkdtemp(prefix="omni-ck-")
        self.addCleanup(shutil.rmtree, self.repo, True)

    def test_empty_cookie_rejected_without_starting_a_browser(self):
        with patch.object(cookies, "_driver") as mock_driver:
            r = cookies.capture_login_from_cookie(self.repo, "   ")
            mock_driver.assert_not_called()
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "empty_cookie")

    def test_valid_cookie_is_verified_headless_and_saved(self):
        fake = _FakeDriver(["https://www.roblox.com/robots.txt",
                            "https://www.roblox.com/home"])
        with patch.object(cookies, "_driver", return_value=fake) as mock_driver, \
             patch.object(cookies, "whoami", return_value=(99, "someuser")):
            r = cookies.capture_login_from_cookie(self.repo, TOKEN,
                                                  on_status=lambda *a: None)
        mock_driver.assert_called_once_with("chrome", headless=True)
        self.assertTrue(fake.quit_called, "browser must be closed either way")
        self.assertEqual(fake.cookies_added[0]["name"], cookies.COOKIE_NAME)
        self.assertEqual(fake.cookies_added[0]["value"], TOKEN)
        self.assertTrue(r["ok"])
        self.assertEqual(r["username"], "someuser")
        self.assertEqual(r["user_id"], 99)
        rec = cookies.get_account(self.repo, "someuser")
        self.assertEqual(rec["cookie"], TOKEN)

    def test_cookie_that_never_authenticates_is_rejected(self):
        fake = _FakeDriver(["https://www.roblox.com/robots.txt",
                            "https://www.roblox.com/login"])
        with patch.object(cookies, "_driver", return_value=fake):
            r = cookies.capture_login_from_cookie(self.repo, TOKEN, timeout=1,
                                                  on_status=lambda *a: None)
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "not_authenticated")
        self.assertIsNone(cookies.get_account(self.repo, "someuser"))

    def test_lands_on_home_but_whoami_fails_is_still_rejected(self):
        """Reaching /home is not proof by itself: users/authenticated must
        also resolve a real user, or a cookie that merely LOOKS accepted
        would get saved and only surface as broken later, in a VM."""
        fake = _FakeDriver(["https://www.roblox.com/robots.txt",
                            "https://www.roblox.com/home"])
        with patch.object(cookies, "_driver", return_value=fake), \
             patch.object(cookies, "whoami", return_value=(None, None)):
            r = cookies.capture_login_from_cookie(self.repo, TOKEN, timeout=1,
                                                  on_status=lambda *a: None)
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "not_authenticated")
        self.assertEqual(cookies.list_accounts(self.repo), [])

    def test_driver_start_failure_is_reported_not_raised(self):
        with patch.object(cookies, "_driver",
                          side_effect=RuntimeError("no chrome installed")):
            r = cookies.capture_login_from_cookie(self.repo, TOKEN)
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "browser_failed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
