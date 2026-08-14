"""Adding an account must FAIL, not crash.

Reported from a real machine (Chrome 151): "Add account" killed the whole app
with PyInstaller's raw "Unhandled exception in script" dialog and a wall of
chromedriver stack addresses. The exception was

    selenium.common.exceptions.WebDriverException: Message: unable to set cookie

out of `drv.add_cookie` in capture_login_from_cookie.

The contract these functions live under is a RESULT DICT: cmd_login and
_capture_and_save_account both read `r["ok"]` and turn a falsy one into a
clean `fail(error, message)`. Every other failure path already honoured that;
the browser calls did not, so one WebDriverException took the process down.

`add_cookie` is bound to the current document's ORIGIN, so it fails with that
bare message whenever the browser is not really on roblox.com — a DNS failure,
a captive portal, a proxy, anything that leaves Chrome on
`chrome-error://chromewebdata/`. Not reproducible on a healthy machine, which
is exactly the signature.
"""
import sys
import types

import pytest

from omnidroid import accounts

pytest.importorskip("selenium")
from selenium.common.exceptions import WebDriverException  # noqa: E402


class FakeDriver:
    """A driver that landed on roblox.com but refuses the cookie."""
    current_url = "https://www.roblox.com/robots.txt"
    cdp_works = False

    def __init__(self):
        self.jar = None

    def get(self, url):
        pass

    def add_cookie(self, spec):
        raise WebDriverException(
            "Message: unable to set cookie\n"
            "  (Session info: chrome=151.0.7922.138)\nStacktrace:\n\tnoise")

    def execute_cdp_cmd(self, cmd, params):
        if not self.cdp_works:
            raise WebDriverException("Message: unable to set cookie")
        self.jar = params

    def get_cookie(self, name):
        return self.jar

    def quit(self):
        pass


@pytest.fixture
def driver(monkeypatch):
    made = {}

    def factory(*a, **k):
        made["drv"] = made.get("drv") or FakeDriver()
        return made["drv"]

    monkeypatch.setattr(accounts, "_driver", factory)
    return made


def test_a_refused_cookie_is_an_error_not_a_crash(driver, tmp_path):
    r = accounts.capture_login_from_cookie(str(tmp_path), "COOKIE",
                                           on_status=lambda *a: None)
    assert r["ok"] is False
    assert r["error"] == "cookie_rejected"
    assert "unable to set cookie" in r["message"]


def test_the_message_is_not_doubled(driver, tmp_path):
    """chromedriver's text already starts with "Message:", and
    WebDriverException.__str__ prepends another — so the naive interpolation
    read "Message: Message: unable to set cookie" and dragged 20 lines of
    chromedriver addresses along behind it."""
    r = accounts.capture_login_from_cookie(str(tmp_path), "COOKIE",
                                           on_status=lambda *a: None)
    assert "Message:" not in r["message"]
    assert "Stacktrace" not in r["message"]
    assert "\n" not in r["message"]


def test_cdp_rescues_a_cookie_add_cookie_refuses(driver, tmp_path, monkeypatch):
    """Network.setCookie writes straight to the network stack and does not
    care which document is loaded, so it survives cases add_cookie cannot."""
    FakeDriver.cdp_works = True
    try:
        # Stop after planting: whoami would need the network.
        monkeypatch.setattr(accounts, "whoami", lambda c: (0, None))
        r = accounts.capture_login_from_cookie(str(tmp_path), "COOKIE",
                                               timeout=0,
                                               on_status=lambda *a: None)
        # Planting SUCCEEDED — the failure has moved to verification.
        assert r["error"] == "not_authenticated"
    finally:
        FakeDriver.cdp_works = False


def test_a_browser_that_never_reached_roblox_says_so(driver, tmp_path, monkeypatch):
    """The real cause behind most "unable to set cookie" reports, and the one
    the user can act on."""
    monkeypatch.setattr(FakeDriver, "current_url", "chrome-error://chromewebdata/")
    r = accounts.capture_login_from_cookie(str(tmp_path), "COOKIE",
                                           on_status=lambda *a: None)
    assert r["error"] == "navigation_failed"
    assert "could not open roblox.com" in r["message"]


def test_navigation_raising_is_handled(driver, tmp_path, monkeypatch):
    def boom(self, url):
        raise WebDriverException("Message: net::ERR_NAME_NOT_RESOLVED")

    monkeypatch.setattr(FakeDriver, "get", boom)
    r = accounts.capture_login_from_cookie(str(tmp_path), "COOKIE",
                                           on_status=lambda *a: None)
    assert r["error"] == "navigation_failed"
    assert "ERR_NAME_NOT_RESOLVED" in r["message"]


def test_interactive_login_navigation_failure_is_handled(driver, tmp_path,
                                                         monkeypatch):
    """The other half of the Add-account menu, same exposure."""
    def boom(self, url):
        raise WebDriverException("Message: net::ERR_CONNECTION_REFUSED")

    monkeypatch.setattr(FakeDriver, "get", boom)
    r = accounts.capture_login(str(tmp_path), timeout=0,
                               on_status=lambda *a: None)
    assert r["ok"] is False
    assert r["error"] == "navigation_failed"


def test_empty_cookie_is_still_rejected_early(tmp_path):
    r = accounts.capture_login_from_cookie(str(tmp_path), "   ")
    assert r["error"] == "empty_cookie"
