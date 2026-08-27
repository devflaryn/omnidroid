"""DEV MODE — point the GUEST's calls at a local omni-backend instead of ours.

The host side of this lives in omni-executor (devserver.py there): it swaps the
origin of every URL the desktop app builds. That is only half the loop. The
other half runs INSIDE the VM: the patched executor's server address is baked
into its native library (spdmteam/github hops rewritten to 72.62.59.232 at bake
time -- see engine.BLOCK_EXTERNAL_HOSTS), so no host-side setting can move it.
It is a compiled-in constant, and rebaking an APK per dev session is not a
"small networking change".

So we move the PACKETS instead of the address. One netfilter rule in the guest:

    iptables -t nat -A OUTPUT -p tcp -d 72.62.59.232 -j DNAT --to-destination 10.0.2.2:5500

Everything in the guest still believes it is talking to 72.62.59.232 -- the URL,
the Host header, the executor's own logging are all unchanged -- but the
connection lands on omni-backend running on the host. 10.0.2.2 is QEMU slirp's
alias for the host's loopback (see qemu_proc.py: `-netdev user,id=net0`), which
is why a dev server on 127.0.0.1 is reachable from inside the VM at all.

TURNING IT ON (env first, then configs/dev.json):

    set OMNI_DEV_SERVER=1                    -> the host's 127.0.0.1:5500
    set OMNI_DEV_SERVER=http://10.0.0.4:5500 -> a backend elsewhere on the LAN

    configs/dev.json:
        {"devServer": "http://127.0.0.1:5500"}

Same variable name the desktop app uses, and omni-executor exports it into
every engine subprocess when its own dev mode is on, so one switch moves both.

NOT IN A RELEASE BUILD: the omni-executor PyInstaller specs -- which are what
freezes this package for customers -- exclude `omnidroid.devserver`, and
engine.py imports it in a try/except. A shipped instance has no module to
import and boots with the production address untouched.
"""

import json
import os
import urllib.parse

# The baked-in production address. This is the only destination we rewrite.
PROD_IP = "72.62.59.232"

# QEMU slirp's alias for the HOST's loopback, as seen from the guest. A dev
# server bound to 127.0.0.1 on the host is 10.0.2.2 from in here; the guest's
# own 127.0.0.1 is the guest, which is why this translation is not optional.
SLIRP_HOST_ALIAS = "10.0.2.2"

# omni-backend's dev port (.env.development.local: PORT=5500).
DEFAULT_DEV_PORT = 5500

DEV_ENV = "OMNI_DEV_SERVER"
DEV_FILE = "dev.json"

_ON = ("1", "true", "yes", "on")
_OFF = ("0", "false", "no", "off")
_LOOPBACK = ("127.0.0.1", "localhost", "::1", "0.0.0.0")


def _normalize(value):
    """A user-typed dev server -> "<host>:<port>" AS THE GUEST MUST DIAL IT.

    Accepts "1", "127.0.0.1:5500", "http://localhost:5500", "10.0.0.4". A
    loopback or wildcard host becomes the slirp alias, because that string is
    about to be handed to iptables inside the VM.
    """
    if not isinstance(value, str):
        return None
    v = value.strip()
    if not v or v.lower() in _OFF:
        return None
    if v.lower() in _ON:
        return f"{SLIRP_HOST_ALIAS}:{DEFAULT_DEV_PORT}"
    parts = urllib.parse.urlsplit(v if "://" in v else "//" + v, scheme="http")
    host = parts.hostname
    if not host:
        return None
    if host.lower() in _LOOPBACK:
        host = SLIRP_HOST_ALIAS
    try:
        port = parts.port or DEFAULT_DEV_PORT
    except ValueError:      # a non-numeric port in the string
        return None
    return f"{host}:{port}"


def dev_target(env=None, config_path=None):
    """Where the guest should be sent, "<host>:<port>", or None when dev mode
    is off. Env wins so one shell can redirect one run."""
    env = os.environ if env is None else env
    raw = str(env.get(DEV_ENV, "")).strip()
    if raw:
        return _normalize(raw)
    if config_path is None:
        from .config import CONFIG_PATH
        config_path = CONFIG_PATH.parent / DEV_FILE
    try:
        with open(config_path, encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or data.get("devMode") is False:
        return None
    return _normalize(data.get("devServer"))


def dnat_script(target):
    """The guest shell that installs the redirect. Idempotent: `-C` tests for
    the rule first, so a re-boot of a warm-pool slot does not stack duplicates.

    OUTPUT (not PREROUTING) because the traffic ORIGINATES in the guest, and
    `-p tcp` with a host:port destination rewrites both in one rule. Failure is
    the caller's to report -- an `iptables` the base does not have, or a kernel
    without the nat table, must not take a boot down with it.
    """
    rule = f"OUTPUT -p tcp -d {PROD_IP} -j DNAT --to-destination {target}"
    return (
        f"iptables -t nat -C {rule} 2>/dev/null || "
        f"iptables -t nat -A {rule} || exit 1; "
        f"iptables -t nat -S OUTPUT 2>/dev/null | grep -c -- '{PROD_IP}'"
    )
