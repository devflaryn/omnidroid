#!/usr/bin/env python3
"""Roblox account cookie manager.

`omni login` opens a real browser at Roblox's login page, YOU sign in (password,
2FA, captcha — all of it stays between you and Roblox), and the moment the
browser lands on roblox.com/home this reads the `.ROBLOSECURITY` cookie and saves
the account under its Roblox USERNAME (auto-detected — no label to type).
`omni play <username> --place <id>` then launches an instance as that account.

Why a browser instead of an HTTP login: Roblox's sign-in is captcha- and
2FA-gated by design. Driving it headlessly would mean defeating those checks;
this deliberately does not. The human does the login, the tool only picks up the
resulting cookie — the same thing you would do by hand with devtools, minus the
copy-paste mistakes.

`omni login --token-file <file>` (or --token/--token-stdin) skips the sign-in
entirely for a cookie you already have — no captcha/2FA to defeat, since you
already completed them elsewhere. It is still verified, just headlessly: the
cookie is loaded into a browser with no window and only trusted once Roblox
actually treats it as an authenticated session, exactly like a fresh interactive
login has to be.

Storage: ONE accounts.json keyed by username, mode 0600, gitignored. A
.ROBLOSECURITY is FULL access to the account — it is never printed, never logged,
and never returned by a --json command.
"""
import json
import os
import time
from pathlib import Path

COOKIE_NAME = ".ROBLOSECURITY"
LOGIN_URL = "https://www.roblox.com/login"
HOME_HOSTS = ("roblox.com",)
HOME_PATHS = ("/home", "/discover", "/games")     # where a login can land
WHOAMI_URL = "https://users.roblox.com/v1/users/authenticated"


# ---------- account store: ONE file, keyed by Roblox username ----------
# All accounts live in a single accounts.json, keyed by the account's Roblox
# USERNAME (the login name from users/authenticated `.name`, NOT the display
# name). This replaces the old cookies/<label>.json-per-account scheme — one file
# is easier to back up, and the username IS the identity used everywhere
# (`omni login` auto-derives it; `omni play <username>` uses it directly).
#
# 0600, gitignored. A .ROBLOSECURITY is full account access.
ACCOUNTS_FILE = "accounts.json"


def accounts_path(repo):
    return Path(repo) / ACCOUNTS_FILE


def _read(repo):
    """The whole store: {"version": 1, "accounts": {username: record}}."""
    p = accounts_path(repo)
    if p.exists():
        try:
            d = json.loads(p.read_text())
            if isinstance(d, dict) and isinstance(d.get("accounts"), dict):
                return d
        except Exception:  # noqa: BLE001
            pass
    return {"version": 1, "accounts": {}}


def _write(repo, data):
    p = accounts_path(repo)
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    try:
        os.chmod(tmp, 0o600)
    except OSError:
        pass          # Windows has no POSIX mode
    os.replace(tmp, p)
    return p


def _migrate_legacy(repo):
    """Fold any old cookies/<label>.json into accounts.json ONCE, keyed by the
    stored username (falling back to the label). Idempotent; leaves the old files
    in place but they are no longer read after this."""
    legacy = Path(repo) / "cookies"
    if not legacy.is_dir():
        return
    data = _read(repo)
    changed = False
    for f in sorted(legacy.glob("*.json")):
        try:
            rec = json.loads(f.read_text())
        except Exception:  # noqa: BLE001
            continue
        if not rec.get("cookie"):
            continue
        key = rec.get("username") or rec.get("label") or f.stem
        if key in data["accounts"]:
            continue          # already migrated / newer wins
        data["accounts"][key] = {
            "username": rec.get("username") or key,
            "user_id": rec.get("user_id"),
            "cookie": rec["cookie"],
            "saved": rec.get("saved") or time.time(),
        }
        changed = True
    if changed:
        _write(repo, data)


def save_account(repo, username, cookie, user_id=None, display_name=None):
    """Upsert an account keyed by username. Previously-set fields that are NOT
    part of a routine cookie refresh (custom_name, place_id, base, proxy, group,
    notes) are PRESERVED across re-logins — a cookie refresh must not wipe
    metadata attached elsewhere."""
    data = _read(repo)
    existing = data["accounts"].get(username) or {}
    data["accounts"][username] = {
        "username": username,
        "user_id": user_id,
        "display_name": display_name,
        "custom_name": existing.get("custom_name"),
        "cookie": cookie,
        "place_id": existing.get("place_id"),
        "base": existing.get("base"),
        "proxy": existing.get("proxy"),
        "group": existing.get("group"),
        "notes": existing.get("notes"),
        "saved": time.time(),
    }
    _write(repo, data)
    return username


def get_account(repo, username):
    """The full record (INCLUDING cookie) for a username, or None."""
    _migrate_legacy(repo)
    return _read(repo)["accounts"].get(username)


def set_custom_name(repo, username, custom_name):
    """Attach (or clear, with an empty/None custom_name) a friendly label to an
    EXISTING account. Display-only: the username stays the account's real
    identity and the instance name — this never renames anything on disk.
    Returns False if no account is saved under that username."""
    data = _read(repo)
    if username not in data["accounts"]:
        return False
    data["accounts"][username]["custom_name"] = custom_name or None
    _write(repo, data)
    return True


def remove_account(repo, username):
    data = _read(repo)
    existed = username in data["accounts"]
    if existed:
        del data["accounts"][username]
        _write(repo, data)
    return existed


def list_accounts(repo):
    """Public view of every saved account — never includes the cookie."""
    _migrate_legacy(repo)
    out = []
    for name, rec in sorted(_read(repo)["accounts"].items()):
        out.append({"username": rec.get("username") or name,
                    "user_id": rec.get("user_id"),
                    "display_name": rec.get("display_name"),
                    "custom_name": rec.get("custom_name"),
                    "place_id": rec.get("place_id"),
                    "base": rec.get("base"),
                    "group": rec.get("group"),
                    "saved": rec.get("saved"),
                    "has_cookie": bool(rec.get("cookie"))})
    return out


_SETTABLE_FIELDS = ("place_id", "base", "proxy", "group", "notes")


def _validate_place_id(v):
    if v is None:
        return None
    try:
        pid = int(str(v).strip())
    except (TypeError, ValueError):
        raise ValueError(f"place_id must be a positive integer, got {v!r}")
    if pid <= 0:
        raise ValueError(f"place_id must be positive, got {pid}")
    return pid


def _validate_base(v):
    if v is None:
        return None
    if v not in ("prod", "dev"):
        raise ValueError(f"base must be 'prod' or 'dev', got {v!r}")
    return v


def set_fields(repo, username, **fields):
    """Update metadata fields on an EXISTING account without touching the
    cookie/identity. Settable: place_id, base, proxy, group, notes. Returns
    False if no account is saved under that username. Raises ValueError on an
    unknown field name or an invalid place_id/base value."""
    for key in fields:
        if key not in _SETTABLE_FIELDS:
            raise ValueError(f"unknown field {key!r}; settable: "
                             f"{', '.join(_SETTABLE_FIELDS)}")
    data = _read(repo)
    if username not in data["accounts"]:
        return False
    rec = data["accounts"][username]
    if "place_id" in fields:
        rec["place_id"] = _validate_place_id(fields["place_id"])
    if "base" in fields:
        rec["base"] = _validate_base(fields["base"])
    for key in ("proxy", "group", "notes"):
        if key in fields:
            rec[key] = fields[key]
    _write(repo, data)
    return True


def whoami(cookie, timeout=20):
    """(user_id, username) for a cookie, or (None, None). Also the cheapest
    proof that a stored cookie is still VALID — Roblox invalidates a cookie when
    the account signs out or changes its password, and a dead cookie otherwise
    only shows up as a login screen inside the VM minutes later.

    Uses curl with the SYSTEM cert store, falling back to urllib. That is not
    fussiness: a python.org install on macOS ships no CA bundle, so plain urllib
    dies with CERTIFICATE_VERIFY_FAILED — and this function would then report a
    perfectly good cookie as INVALID, which is the most misleading answer it
    could possibly give. (omni.py's downloader prefers curl for the same reason.)

    The cookie goes in a curl --config file, never in argv: an argv cookie is
    readable by every process on the host via ps.
    """
    import shutil as _sh
    import subprocess
    import tempfile

    curl = _sh.which("curl")
    if curl:
        tmp = Path(tempfile.mkdtemp(prefix="omni-whoami-"))
        try:
            cfg = tmp / "curl.cfg"
            cfg.write_text(
                f'header = "Cookie: {COOKIE_NAME}={cookie}"\n'
                f'header = "User-Agent: Mozilla/5.0"\n'
                f'url = "{WHOAMI_URL}"\n'
                f"silent\nfail\n", encoding="utf-8")
            try:
                os.chmod(cfg, 0o600)
            except OSError:
                pass
            r = subprocess.run([curl, "--config", str(cfg)],
                               capture_output=True, text=True, timeout=timeout)
            if r.returncode == 0 and r.stdout:
                try:
                    d = json.loads(r.stdout)
                    return d.get("id"), d.get("name")
                except ValueError:
                    return None, None
            return None, None
        except Exception:  # noqa: BLE001
            pass
        finally:
            import shutil as _s2
            _s2.rmtree(tmp, ignore_errors=True)

    import urllib.request
    req = urllib.request.Request(
        WHOAMI_URL,
        headers={"Cookie": f"{COOKIE_NAME}={cookie}",
                 "User-Agent": "Mozilla/5.0"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            d = json.loads(r.read().decode())
        return d.get("id"), d.get("name")
    except Exception:  # noqa: BLE001
        return None, None


def _chrome_binary():
    """Best-effort path to the installed Chrome/Chromium, per-OS."""
    import platform
    import shutil
    sysname = platform.system()
    if sysname == "Darwin":
        cands = ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                 "/Applications/Chromium.app/Contents/MacOS/Chromium"]
    elif sysname == "Windows":
        import os as _os
        pf = _os.environ.get("ProgramFiles", r"C:\Program Files")
        pf86 = _os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")
        cands = [pf + r"\Google\Chrome\Application\chrome.exe",
                 pf86 + r"\Google\Chrome\Application\chrome.exe"]
    else:
        cands = []
        for n in ("google-chrome", "google-chrome-stable", "chromium",
                  "chromium-browser", "chrome"):
            p = shutil.which(n)
            if p:
                cands.append(p)
    for c in cands:
        if os.path.exists(c):
            return c
    return None


def _browser_version(binary):
    import re
    import subprocess
    try:
        out = subprocess.run([binary, "--version"], capture_output=True,
                             text=True, timeout=15).stdout
    except Exception:  # noqa: BLE001
        return None
    m = re.search(r"(\d+\.\d+\.\d+\.\d+)", out or "")
    return m.group(1) if m else None


def _resolve_chromedriver():
    """A chromedriver matching the INSTALLED Chrome, via Selenium Manager's
    cache — never a stale one on PATH.

    A brew-cask chromedriver pinned to an old Chrome is a common macOS state, and
    Selenium (even Selenium Manager) prefers a driver found on PATH, only WARNING
    on a version mismatch. Against a newer Chrome that surfaces as the fatal
    "session not created: This version of ChromeDriver only supports Chrome
    version N". Passing the detected --browser-version forces Selenium Manager to
    download and return the correct driver from ~/.cache/selenium instead.

    Returns (driver_path, browser_path) or (None, None) to fall back to default
    resolution.
    """
    try:
        from selenium.webdriver.common.selenium_manager import SeleniumManager
    except Exception:  # noqa: BLE001
        return None, None
    args = ["--browser", "chrome"]
    binary = _chrome_binary()
    if binary:
        ver = _browser_version(binary)
        if ver:
            args += ["--browser-version", ver]
        args += ["--browser-path", binary]
    try:
        paths = SeleniumManager().binary_paths(args)
        return paths.get("driver_path"), paths.get("browser_path")
    except Exception:  # noqa: BLE001
        return None, None


def _driver(browser, profile_dir=None, headless=False):
    from selenium import webdriver
    if browser == "firefox":
        opts = webdriver.FirefoxOptions()
        if headless:
            opts.add_argument("-headless")
        return webdriver.Firefox(options=opts)
    opts = webdriver.ChromeOptions()
    opts.add_argument("--window-size=1200,900")
    if headless:
        # "new" headless mode renders like a real Chrome build; the legacy
        # --headless flag uses a different renderer Roblox is more likely to
        # bot-flag. There is no human here to see the window either way.
        opts.add_argument("--headless=new")
    if profile_dir:
        opts.add_argument(f"--user-data-dir={profile_dir}")
    driver_path, browser_path = _resolve_chromedriver()
    if browser_path:
        opts.binary_location = browser_path
    if driver_path:
        from selenium.webdriver.chrome.service import Service
        return webdriver.Chrome(service=Service(executable_path=driver_path),
                                options=opts)
    # Nothing resolved — let Selenium's own default path try (works when PATH is
    # clean or no driver is installed at all).
    return webdriver.Chrome(options=opts)


def capture_login(repo, browser="chrome", timeout=300, profile_dir=None,
                  poll=1.0, on_status=print):
    """Drive a browser login and save the account keyed by its Roblox USERNAME.

    No label is asked for: the account IS its username, and the username is read
    from users/authenticated (`.name`, the login name — not the display name)
    the moment the session is confirmed. Returns a result dict.

    Success is NOT "the URL changed" — a login can bounce through
    /login?ReturnUrl=..., 2FA, or a captcha and still not be authenticated. The
    cookie is only accepted once it exists AND that endpoint confirms it resolves
    to a real user (which is also where the username comes from).
    """
    try:
        from selenium.common.exceptions import WebDriverException  # noqa: F401
    except ImportError:
        return {"ok": False, "error": "selenium_missing",
                "message": ("selenium is not installed: pip install selenium "
                            "(and have Chrome or Firefox available)")}
    try:
        drv = _driver(browser, profile_dir)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": "browser_failed",
                "message": f"could not start {browser}: {e}"}

    try:
        drv.get(LOGIN_URL)
        on_status(f"[login] browser open at {LOGIN_URL} — sign in "
                  f"(password/2FA/captcha all stay between you and Roblox).")
        on_status(f"[login] waiting up to {timeout}s for a landing on "
                  f"roblox.com{'|'.join(HOME_PATHS)} ...")
        deadline = time.time() + timeout
        seen_home = False
        while time.time() < deadline:
            try:
                url = drv.current_url or ""
            except Exception:  # noqa: BLE001 — window closed
                return {"ok": False, "error": "browser_closed",
                        "message": "the browser window was closed before login "
                                   "completed"}
            landed = (any(h in url for h in HOME_HOSTS)
                      and any(p in url for p in HOME_PATHS))
            if landed and not seen_home:
                seen_home = True
                on_status(f"[login] landed on {url.split('?')[0]} — reading "
                          f"the session cookie")
            if seen_home:
                c = None
                try:
                    c = drv.get_cookie(COOKIE_NAME)
                except Exception:  # noqa: BLE001
                    pass
                if c and c.get("value"):
                    val = c["value"]
                    uid, uname = whoami(val)
                    if uid and uname:
                        save_account(repo, uname, val, uid)
                        on_status(f"[login] authenticated as {uname} ({uid}); "
                                  f"saved to {ACCOUNTS_FILE} (0600)")
                        return {"ok": True, "username": uname, "user_id": uid,
                                "path": str(accounts_path(repo))}
                    # Cookie present but not yet authenticated (mid-2FA):
                    # keep waiting rather than saving a useless value.
            time.sleep(poll)
        why = ("reached home but the cookie never validated — 2FA unfinished?"
               if seen_home else "never reached roblox.com/home")
        return {"ok": False, "error": "timeout",
                "message": f"no authenticated session within {timeout}s: {why}"}
    finally:
        try:
            drv.quit()
        except Exception:  # noqa: BLE001
            pass


def capture_login_from_cookie(repo, cookie, browser="chrome", timeout=30,
                              on_status=print):
    """Adopt an ALREADY-OBTAINED `.ROBLOSECURITY` cookie instead of driving an
    interactive sign-in — e.g. one exported from another browser/device.

    HEADLESS, not visible: unlike `capture_login` there is no human step here,
    so there is no window to show. The browser exists only to prove the cookie
    behaves like a real authenticated session (planted the same way the
    product's bootstrap plants it into Roblox's own WebView jar — see
    contracts/omni-session.md) rather than trusting a bare HTTP call alone.
    Held to the same bar as an interactive login: reaching roblox.com/home is
    not enough by itself, `users/authenticated` must also resolve a real user
    — that is also where the username (the account key) and user_id come from.
    Saved the same way: keyed by username in accounts.json.
    """
    cookie = (cookie or "").strip()
    if not cookie:
        return {"ok": False, "error": "empty_cookie",
                "message": "no cookie value given"}
    try:
        from selenium.common.exceptions import WebDriverException  # noqa: F401
    except ImportError:
        return {"ok": False, "error": "selenium_missing",
                "message": ("selenium is not installed: pip install selenium "
                            "(and have Chrome or Firefox available)")}
    try:
        drv = _driver(browser, headless=True)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": "browser_failed",
                "message": f"could not start headless {browser}: {e}"}

    try:
        # add_cookie() only works once the browser is already on a matching
        # domain — robots.txt is the cheapest real page roblox.com serves.
        drv.get("https://www.roblox.com/robots.txt")
        drv.add_cookie({"name": COOKIE_NAME, "value": cookie,
                        "domain": ".roblox.com", "path": "/", "secure": True})
        on_status(f"[login] headless {browser} verifying the cookie ...")
        drv.get("https://www.roblox.com/home")
        deadline = time.time() + timeout
        landed = False
        while time.time() < deadline:
            try:
                url = drv.current_url or ""
            except Exception:  # noqa: BLE001 — driver died mid-check
                return {"ok": False, "error": "browser_closed",
                        "message": "the headless browser session ended "
                                   "before verification completed"}
            if any(h in url for h in HOME_HOSTS) and any(p in url for p in HOME_PATHS):
                landed = True
                break
            time.sleep(0.5)
        if not landed:
            return {"ok": False, "error": "not_authenticated",
                    "message": "the cookie did not reach an authenticated "
                               "roblox.com page (redirected to /login, or "
                               "timed out) — it is likely invalid or expired"}
        uid, uname = whoami(cookie)
        if not (uid and uname):
            return {"ok": False, "error": "not_authenticated",
                    "message": "the browser session looked authenticated but "
                               "users/authenticated did not resolve a real "
                               "user for this cookie"}
        save_account(repo, uname, cookie, uid)
        on_status(f"[login] verified + authenticated as {uname} ({uid}); "
                  f"saved to {ACCOUNTS_FILE} (0600)")
        return {"ok": True, "username": uname, "user_id": uid,
                "path": str(accounts_path(repo))}
    finally:
        try:
            drv.quit()
        except Exception:  # noqa: BLE001
            pass
