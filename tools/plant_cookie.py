#!/usr/bin/env python3
"""Put a Roblox session cookie into an Android app's own WebView cookie store -- the Chromium
`Cookies` database that `android.webkit.CookieManager` keeps at
`/data/data/<package>/app_webview/Default/Cookies` -- as a device that had signed in holds it.

    python tools/plant_cookie.py <Cookies database> <cookie file>

The cookie file holds the `.ROBLOSECURITY` value: bare, as `.ROBLOSECURITY=<value>`, or as a
Netscape `cookies.txt`. The value is never printed. The app must not be running (`am force-stop`):
the database is changed in place, without a journal file, so the files the app owns stay the app's.
Stdlib only.
"""
import sqlite3
import sys
import time

NAME = ".ROBLOSECURITY"


def value_of(path):
    text = open(path, encoding="utf-8", errors="replace").read().strip()
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") and not line.startswith("#HttpOnly_"):
            continue
        fields = line.split("\t")
        if len(fields) >= 7 and fields[5] == NAME:
            return fields[6].strip()
        if line.startswith(NAME + "="):
            return line[len(NAME) + 1 :].split(";")[0].strip()
    return text.splitlines()[0].strip()


def chromium_now():
    # Microseconds since 1601-01-01, Chromium's base::Time.
    return int((time.time() + 11644473600) * 1_000_000)


def main():
    db, cookie = sys.argv[1], sys.argv[2]
    value = value_of(cookie)
    if not value.startswith("_|WARNING"):
        sys.exit("plant_cookie: the file does not hold a .ROBLOSECURITY value")
    now = chromium_now()
    year = 365 * 24 * 3600 * 1_000_000
    c = sqlite3.connect(f"file:{db}?mode=rw", uri=True, isolation_level=None)
    c.execute("PRAGMA journal_mode=OFF")
    version = c.execute("SELECT value FROM meta WHERE key='version'").fetchone()
    c.execute("BEGIN")
    c.execute("DELETE FROM cookies WHERE name=? AND host_key LIKE '%roblox.com'", (NAME,))
    c.execute(
        "INSERT INTO cookies (creation_utc, host_key, top_frame_site_key, name, value, encrypted_value,"
        " path, expires_utc, is_secure, is_httponly, last_access_utc, has_expires, is_persistent,"
        " priority, samesite, source_scheme, source_port, last_update_utc)"
        " VALUES (?, '.roblox.com', '', ?, ?, x'', '/', ?, 1, 1, ?, 1, 1, 1, -1, 2, 443, ?)",
        (now, NAME, value, now + year, now, now),
    )
    c.execute("COMMIT")
    n = c.execute("SELECT count(*) FROM cookies WHERE name=?", (NAME,)).fetchone()[0]
    c.close()
    print(f"plant_cookie: {NAME} planted for .roblox.com (store version {version[0] if version else '?'}, {n} row)")


if __name__ == "__main__":
    main()
