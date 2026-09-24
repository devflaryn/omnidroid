"""`omnidroid login`: sign in to Roblox in a real Chromium window and keep the session.

Run by the launcher (crates/omnidroid) with the Python environment it prepares; not meant to be
run by hand. Opens Roblox's login page in a fresh browser profile (nothing from the person's own
browser), and either lets the person sign in or -- given a username and a password -- fills the
form and submits it. Whatever the site then asks (a captcha, a 2-step code) stays in the window
for the person to answer. Once the page reaches /home, it keeps:

  <dir>/<name>.txt         the .ROBLOSECURITY cookie, the file `omnidroid play --cookie` reads
  <dir>/<name>.login.json  the username and password that were entered, for a later
                           `omnidroid login <name>`

<name> is the account's own name, asked of Roblox with the new cookie. Both files are readable
by this user only. Neither the cookie nor the password is ever printed.

The password comes in the environment (OMNI_LOGIN_PASSWORD), never on this command line.

The browser is omnidroid's own Chromium (Chrome for Testing), not one installed on the computer:
Selenium Manager fetches it and its driver into SE_CACHE_PATH, which the launcher points at
<app-data>/../chromium. On a Linux that restricts unprivileged user namespaces (Ubuntu 23.10+,
AppArmor), a browser outside /opt/google/chrome or /usr/lib/chromium cannot start its sandbox, and
only root can allow it; there it runs with --no-sandbox, and says so.
"""

import argparse
import json
import os
import sys
import time
import urllib.parse
import urllib.request

from selenium import webdriver
from selenium.common.exceptions import WebDriverException
from selenium.webdriver.common.by import By

LOGIN_URL = "https://www.roblox.com/login"
AUTHENTICATED_URL = "https://users.roblox.com/v1/users/authenticated"
# How long a person has to finish signing in (a captcha, a 2-step code) before this gives up.
WAIT_SECONDS = 15 * 60

USERNAME_FIELDS = "#login-username, input[name='username'], input[autocomplete='username']"
PASSWORD_FIELDS = "#login-password, input[name='password'], input[type='password']"
SUBMIT_BUTTONS = "#login-button, button[type='submit']"

# The two fields' values as the page holds them: kept while the person types, so what was
# entered is known once the page has moved on to /home.
READ_FIELDS = """
const pick = (s) => { const e = document.querySelector(s); return e ? e.value : null; };
return [pick(arguments[0]), pick(arguments[1])];
"""


def userns_restricted():
    """Whether AppArmor keeps this Chromium from the user namespaces its sandbox needs."""
    try:
        with open("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", encoding="ascii") as f:
            return f.read().strip() == "1"
    except OSError:
        return False


def say(message):
    print(f"omnidroid login: {message}", flush=True)


def at_home(url):
    parts = urllib.parse.urlparse(url)
    host = (parts.hostname or "").lower()
    return (host == "roblox.com" or host.endswith(".roblox.com")) and parts.path.lower().startswith("/home")


def first_visible(driver, selector):
    for element in driver.find_elements(By.CSS_SELECTOR, selector):
        try:
            if element.is_displayed() and element.is_enabled():
                return element
        except WebDriverException:
            pass
    return None


def fill_and_submit(driver, username, password):
    """Type the username and password as a person would, and press the login button.

    Also covers a two-step form (the username, then the password on the next screen)."""
    deadline = time.time() + 60
    typed_username = typed_password = False
    while time.time() < deadline and not typed_password:
        user_field = first_visible(driver, USERNAME_FIELDS)
        pass_field = first_visible(driver, PASSWORD_FIELDS)
        if user_field is not None and not typed_username:
            user_field.clear()
            user_field.send_keys(username)
            typed_username = True
        if pass_field is not None and typed_username:
            pass_field.clear()
            pass_field.send_keys(password)
            typed_password = True
        button = first_visible(driver, SUBMIT_BUTTONS)
        if button is not None and typed_username:
            try:
                button.click()
            except WebDriverException:
                pass
        if not typed_password:
            time.sleep(1)
    if not typed_password:
        say("the login form was not found in 60 s -- finish signing in in the window")


def account_name(cookie):
    """The signed-in account's name, from Roblox itself, with the new cookie."""
    request = urllib.request.Request(AUTHENTICATED_URL, headers={"Cookie": f".ROBLOSECURITY={cookie}"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)["name"]


def write_private(path, text):
    """Write `text` to `path`, readable and writable by this user only."""
    partial = path + ".partial"
    fd = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(partial, path)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dir", required=True, help="where the cookie and the login are kept")
    parser.add_argument("--username", help="fill the form with this username (and OMNI_LOGIN_PASSWORD)")
    args = parser.parse_args()
    password = os.environ.get("OMNI_LOGIN_PASSWORD") or None

    options = webdriver.ChromeOptions()
    options.add_argument("--window-size=1100,900")
    # Chrome for Testing, fetched by Selenium Manager into omnidroid's own folder (SE_CACHE_PATH),
    # rather than whatever Chrome the computer has; the profile is a fresh one each time.
    options.browser_version = "stable"
    if sys.platform.startswith("linux") and userns_restricted():
        options.add_argument("--no-sandbox")
        say("this Linux restricts user namespaces (AppArmor), so the login browser runs without "
            "Chrome's sandbox -- it only visits roblox.com, in a fresh profile")
    say("opening Chromium on Roblox's login page (fetched once, the first time)")
    driver = webdriver.Chrome(options=options)
    try:
        driver.get(LOGIN_URL)
        if args.username and password:
            say(f"signing in as {args.username} -- answer anything the page asks in the window")
            fill_and_submit(driver, args.username, password)
        else:
            say("sign in in the window; this waits until the page reaches /home")
        entered = [args.username if args.username and password else None, password]
        deadline = time.time() + WAIT_SECONDS
        while not at_home(driver.current_url):
            if time.time() > deadline:
                say(f"the page did not reach /home in {WAIT_SECONDS // 60} minutes; nothing was saved")
                return 1
            try:
                values = driver.execute_script(READ_FIELDS, USERNAME_FIELDS, PASSWORD_FIELDS)
                for index, value in enumerate(values):
                    if value:
                        entered[index] = value
            except WebDriverException:
                pass  # between pages
            time.sleep(0.25)
        cookie = next((c["value"] for c in driver.get_cookies() if c["name"] == ".ROBLOSECURITY"), None)
    finally:
        driver.quit()

    if not cookie:
        say("the page reached /home but holds no .ROBLOSECURITY cookie; nothing was saved")
        return 1
    try:
        name = account_name(cookie)
    except Exception as error:  # noqa: BLE001 -- any failure means the name is not known
        name = entered[0]
        if not name:
            say(f"Roblox did not say whose account this is ({error}); nothing was saved")
            return 1
        say(f"Roblox did not say whose account this is ({error}); using {name!r}")
    os.makedirs(args.dir, exist_ok=True)
    cookie_file = os.path.join(args.dir, f"{name}.txt")
    write_private(cookie_file, cookie + "\n")
    say(f"signed in as {name}; the cookie is in {cookie_file}")
    login_file = os.path.join(args.dir, f"{name}.login.json")
    if entered[1]:
        login = {"username": name, "login": entered[0] or name, "password": entered[1]}
        write_private(login_file, json.dumps(login, indent=2) + "\n")
        say(f"the username and password are in {login_file}")
    else:
        say("no password was typed (Quick Sign-in?), so only the cookie was kept")
    say(f"play as this account: omnidroid play --cookie {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
