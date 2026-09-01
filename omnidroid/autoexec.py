"""Push the host's autoexec scripts to the exec server for one launch.

THE HOST IS THE SOURCE. Every file the user drops in `<data_dir>/autoexec/`
is a Luau script that should run automatically in every instance, once, at
session start. This module reads that directory and POSTs its contents to the
omni-backend exec bridge (`/omni/exec/autoexec/set`) keyed by the account's
Roblox username -- the same `channel` the in-game bridge polls under. The
in-game menu GETs the bundle at session start and runLuau()s each script.

WHY PUSH PER LAUNCH RATHER THAN BAKE. The user edits the host dir between runs
and expects the next launch to reflect it. The push is a few KB and runs once
per boot beside the consent/execmark grants, so a change on disk is live on the
very next launch with nothing to rebuild. An EMPTY dir pushes an empty list,
which CLEARS the channel server-side -- deleting every file has to actually
stop them running, not leave the last set stuck.

WHY OVER HTTP, NOT INTO THE GUEST. This build's executor does not reliably read
files the host writes into its workspace (see execmark.py's "fails open" note),
so autoexec rides the one channel the executor always has: game:HttpGet against
our server. That makes this a HOST-SIDE network call, not an adb step -- unlike
execmark/consent/awake, which only build a guest command for the engine to run.

Best-effort and LOUD either way: a failed push must never fail a boot, but a
silent failure would look like "autoexec is broken" with no clue, so both the
success and the failure print one line.
"""
import json
import os
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# The kill switch, mirroring OMNI_NO_CONSENT / OMNI_NO_EXECMARK.
#
# ⚠ IT DOES NOT MEAN "SKIP THE PUSH", and that distinction is the whole
# difference between a switch that works and one that looks like it does.
# The scripts do not live on this machine at run time -- they live in the
# exec server's channel for this account, put there by the LAST launch that
# pushed. Skipping the push therefore leaves the previous bundle in place and
# the instance runs it anyway. So "disabled" pushes an EMPTY bundle, which is
# exactly what an empty folder already does (see push_autoexec's own note:
# "deleting every file has to actually stop them running, not leave the last
# set stuck"). Same rule, same code path.
NO_AUTOEXEC_ENV = "OMNI_NO_AUTOEXEC"

# One script turned off WITHOUT deleting it. A trailing `.disabled` is the
# marker, chosen over a leading `_` or a subfolder because filename order IS
# run order here: `20-loot.lua` -> `20-loot.lua.disabled` keeps its place in
# the sequence, so switching it back on cannot silently reorder the rest.
DISABLED_SUFFIX = ".disabled"

# The shared secret the /autoexec/set route checks. Must match the backend's
# OMNI_EXEC_ADMIN_SECRET (same default there, overridable in both places).
ADMIN_SECRET_ENV = "OMNI_EXEC_ADMIN_SECRET"
_DEFAULT_ADMIN_SECRET = "omni-autoexec-dev-6f3a91"

# Files this large are not scripts anyone meant to autoexec; skip them so a
# stray asset dropped in the folder cannot bloat the push.
_MAX_FILE_BYTES = 200_000
_MAX_FILES = 25

# Extensions we treat as scripts. Empty extension is allowed too -- github's
# extensionless script sources arrive that way -- but binaries are skipped by
# the NUL-byte sniff below rather than by an extension allowlist.
_SKIP_EXT = {".png", ".jpg", ".jpeg", ".webp", ".gif", ".zip", ".gz",
             ".exe", ".dll", ".so", ".ico", ".bmp", ".ttf", ".otf"}


def autoexec_enabled(env=None):
    """False when the kill switch is set to a truthy value."""
    val = (env or os.environ).get(NO_AUTOEXEC_ENV, "")
    return str(val).strip().lower() not in ("1", "true", "yes", "on")


def is_disabled(name):
    """True for a script the user has switched off (see DISABLED_SUFFIX)."""
    return str(name).lower().endswith(DISABLED_SUFFIX)


def enabled_name(name):
    """`foo.lua.disabled` -> `foo.lua`. Unchanged if it is not disabled."""
    return str(name)[:-len(DISABLED_SUFFIX)] if is_disabled(name) else str(name)


def disabled_name(name):
    """`foo.lua` -> `foo.lua.disabled`. Unchanged if it already is."""
    return str(name) if is_disabled(name) else str(name) + DISABLED_SUFFIX


def autoexec_dir(data_dir):
    """`<data_dir>/autoexec/`. Created on demand so the folder always exists for
    the user to drop files into (the GUI's "open autoexec folder" lands here)."""
    d = Path(data_dir) / "autoexec"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


def read_scripts(data_dir):
    """Every script in the autoexec dir, filename order, as [{name, body}].

    Ordering is the user's lever exactly like the ui/ modules: a `10_` prefix
    runs before `20_`. Directories, oversized files, unreadable files and
    anything that sniffs as binary (a NUL byte in the head) are skipped rather
    than failing the batch."""
    d = autoexec_dir(data_dir)
    out = []
    try:
        names = sorted(p for p in d.iterdir() if p.is_file())
    except OSError:
        return out
    for p in names:
        if is_disabled(p.name):            # switched off, not deleted
            continue
        if p.suffix.lower() in _SKIP_EXT:
            continue
        try:
            if p.stat().st_size > _MAX_FILE_BYTES:
                continue
            raw = p.read_bytes()
        except OSError:
            continue
        if b"\x00" in raw[:1024]:          # binary; not a script
            continue
        body = raw.decode("utf-8", errors="replace")
        if not body.strip():
            continue
        out.append({"name": p.name, "body": body})
        if len(out) >= _MAX_FILES:
            break
    return out


def server_base(cfg):
    """The exec server's origin (scheme://host[:port]).

    Derived from the same `qemu.download_url` the engine already trusts for
    delivery, so autoexec talks to whatever server this deployment uses without
    a second config knob. OMNI_EXEC_BASE overrides for a split deployment."""
    override = os.environ.get("OMNI_EXEC_BASE")
    if override:
        return override.rstrip("/")
    url = ((cfg or {}).get("qemu", {}) or {}).get("download_url") or ""
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme and parsed.netloc:
        return f"{parsed.scheme}://{parsed.netloc}"
    return "http://72.62.59.232"


def admin_secret():
    return os.environ.get(ADMIN_SECRET_ENV) or _DEFAULT_ADMIN_SECRET


def push_autoexec(channel, cfg, data_dir, label, timeout=10):
    """POST the host autoexec bundle for `channel`. Returns the count pushed, or
    None on failure. Never raises -- a boot must not fail over autoexec wiring.

    Prints one line either way. An empty dir is a valid state: it pushes an
    empty list, which clears the channel, and reports '0 scripts (cleared)'."""
    # DISABLED PUSHES AN EMPTY BUNDLE rather than returning early -- see
    # NO_AUTOEXEC_ENV. Returning here would leave the previous launch's
    # scripts live in the server-side channel, so the switch would appear to
    # do nothing.
    off = not autoexec_enabled()
    scripts = [] if off else read_scripts(data_dir)
    base = server_base(cfg)
    payload = json.dumps({"channel": channel, "scripts": scripts}).encode("utf-8")
    req = urllib.request.Request(
        f"{base}/omni/exec/autoexec/set",
        data=payload,
        method="POST",
        headers={"Content-Type": "application/json",
                 "x-omni-admin": admin_secret()},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode("utf-8", errors="replace")
    except (urllib.error.URLError, OSError) as e:
        print(f"[{label}] autoexec push FAILED ({e}); no autoexec this session")
        return None
    try:
        data = json.loads(body)
        count = int(data.get("count", len(scripts)))
    except (ValueError, TypeError):
        count = len(scripts)
    if off:
        print(f"[{label}] autoexec: OFF — the channel was cleared, so nothing "
              f"auto-runs this session (turn it back on in the app, or unset "
              f"{NO_AUTOEXEC_ENV})")
    elif count == 0:
        print(f"[{label}] autoexec: 0 scripts (cleared) — drop .lua files in "
              f"{autoexec_dir(data_dir)} to auto-run them")
    else:
        print(f"[{label}] autoexec: pushed {count} script"
              f"{'' if count == 1 else 's'} for {channel} "
              f"(run at session start)")
    return count
