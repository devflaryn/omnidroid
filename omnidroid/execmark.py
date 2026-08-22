# omnidroid/execmark.py
"""Tell the in-game executor menu that it is running inside Omnidroid.

THE WHOLE MECHANISM IS ONE FILE. The menu served from `/gist`
(omni-backend/backend/src/omni-exec/payloads/ui/00_prelude.lua) asks

    isfile("omni_host.data")

and branches on the answer: a marker means an unattended Omnidroid guest, so
it announces itself with a card that dismisses itself and leaves NOTHING
parked over the game; no marker means somebody's phone, so it leaves a
floating button on screen permanently. `MARKER_NAME` here and `OMNI.MARKER`
there are one contract with no runtime negotiation between them, and
`backend/tests/omniExecUi.test.js` is the only place the two halves are ever
checked against each other.

WHY A LIST OF ROOTS RATHER THAN A PATH. `isfile` resolves against the
executor's own workspace, not against `/` — the stock Arceus gist calls it the
same way (`isfile("warning.data")`), which is what fixes the convention. But
Arceus X NEO *is* the patched `com.roblox.client`, its workspace is somewhere
under that package's storage, and nothing in this tree names it. So the marker
is written into EVERY plausible root. The file is 20 bytes and the write is
idempotent, so being wrong about six of seven candidates costs nothing, while
being wrong about the seventh costs the feature. `omnidroid exec-mark --probe`
resolves it as fact against a live instance; when it does, the answer belongs
at the top of CANDIDATE_ROOTS rather than replacing the list — a future APK
can move its workspace again.

DETECTION FAILS OPEN, and that direction is the point. Every step here is
best-effort: a root that cannot be created is skipped, a chown that is refused
is ignored, and the whole thing ends in `true` so a boot is never failed by
it. A missed marker costs one unwanted button on a farming instance; the
inverse default would cost a paying customer their only route into the menu.

Like farming.py, awake.py and consent.py this module only BUILDS the guest
command; it runs no adb and no subprocess of its own. The engine applies it
(`write_exec_marker`, every boot, beside the consent grants).
"""
import re
import shlex

# `sh` is farming's, and stays farming's on purpose: the adb-shell quoting
# trap it documents (adb re-parses a joined argv, so an unquoted `a; b` runs a
# fragment of itself and still reports success) is a property of the
# transport, not of any one module.
from omnidroid.farming import sh


# The environment kill switch, mirroring OMNI_NO_CONSENT / OMNI_NO_WARM. Set
# it to make an Omnidroid guest present itself as a generic device, which is
# how the phone/emulator branch of the menu gets exercised without a phone.
NO_EXECMARK_ENV = "OMNI_NO_EXECMARK"

# The contract with OMNI.MARKER in 00_prelude.lua. Changing it here without
# changing it there silently disables the Omnidroid branch of the menu.
MARKER_NAME = "omni_host.data"

# Printed by the guest script so the engine can report how many roots took it.
MARK_PREFIX = "OMNI_EXECMARK"

_COUNT_RE = re.compile(rf"{MARK_PREFIX}\s+(\d+)\s+(\d+)")


def execmark_enabled(env=None):
    """False when the kill switch is set to a truthy value."""
    val = (env or {}).get(NO_EXECMARK_ENV, "")
    return str(val).strip().lower() not in ("1", "true", "yes", "on")


def candidate_roots(game_pkg=None):
    """Every directory the executor might resolve a relative path against.

    Ordered most-likely-first purely as documentation; the script writes all of
    them and the order has no runtime meaning.

    The `Android/data/<pkg>/files` entry is the one that is easy to leave out
    and matters most on a modern target: it is the app's EXTERNAL files dir,
    reachable without MANAGE_EXTERNAL_STORAGE, which is where a scoped-storage
    build has to keep a workspace it can still write after targeting API 30.
    """
    pkg = game_pkg or "com.roblox.client"
    return [
        f"/data/data/{pkg}/files/exe/workspace",
        f"/data/data/{pkg}/files/exe",
        f"/data/data/{pkg}/files/workspace",
        f"/data/data/{pkg}/files",
        f"/storage/emulated/0/Android/data/{pkg}/files/workspace",
        "/storage/emulated/0/Arceus X/Workspace",
        "/storage/emulated/0/Arceus X NEO/Workspace",
        "/storage/emulated/0/Delta/Workspace",
    ]


def marker_body(mode=None):
    """What the marker holds.

    The payload treats ANY readable content as optional detail and shows it on
    the status page, so this is a label rather than a format: `omnidroid` alone
    is already a complete answer, and the mode is the one thing the menu cannot
    work out for itself.
    """
    return f"omnidroid:{mode}" if mode else "omnidroid"


def build_marker_script(mode=None, game_pkg=None):
    """One adb step writing the marker into every candidate root.

    THE OWNERSHIP DANCE IS NOT OPTIONAL, and it is the part that would fail
    silently. adb is uid 0 on the x86 base, so every directory and file this
    creates under `/data/data/<pkg>` is created as ROOT — and the executor runs
    as the app's own uid, which then cannot read a 0600 root-owned file inside
    a directory it does not own. `isfile` would answer false with the marker
    sitting right there.

    So each created path is handed to whoever owns the package directory
    (`stat -c %u:%g`, the app's uid), the file is left world-readable, and
    `restorecon` is asked to put the SELinux label back — an app-data file
    created by root gets the shell's label, and SEAndroid denies the app's read
    on the wrong label even when the Unix mode allows it.

    Everything is `>/dev/null 2>&1`-guarded and the script ends in `true`: a
    root that cannot be created, a chown the kernel refuses, or a missing
    `restorecon` must not abort the remaining roots or fail the boot.
    """
    roots = " ".join(shlex.quote(r) for r in candidate_roots(game_pkg))
    body = shlex.quote(marker_body(mode))
    name = shlex.quote(MARKER_NAME)
    pkgdir = shlex.quote(f"/data/data/{game_pkg or 'com.roblox.client'}")

    return sh(
        # Who owns the package? Empty on a guest where it is not installed,
        # which is fine — the chown is then skipped and the sdcard roots (which
        # are on a uid-less filesystem anyway) still work.
        f"OWN=$(stat -c '%u:%g' {pkgdir} 2>/dev/null); "
        f"N=0; OK=0; "
        f"for R in {roots}; do "
        f"N=$((N+1)); "
        f"mkdir -p \"$R\" >/dev/null 2>&1 || continue; "
        f"echo {body} > \"$R\"/{name} 2>/dev/null || continue; "
        f"chmod 0755 \"$R\" >/dev/null 2>&1; "
        f"chmod 0644 \"$R\"/{name} >/dev/null 2>&1; "
        f"[ -n \"$OWN\" ] && chown \"$OWN\" \"$R\" \"$R\"/{name} >/dev/null 2>&1; "
        f"restorecon -R \"$R\" >/dev/null 2>&1; "
        f"OK=$((OK+1)); "
        f"done; "
        f"echo {MARK_PREFIX} $OK $N; true"
    )


def build_marker_probe(game_pkg=None):
    """Ask a live guest which roots actually hold the marker.

    Reads the state BACK rather than trusting the write, for the same reason
    consent.build_consent_probe does: a `mkdir -p` onto a read-only mount exits
    0 on some builds and creates nothing.
    """
    roots = " ".join(shlex.quote(r) for r in candidate_roots(game_pkg))
    name = shlex.quote(MARKER_NAME)
    return sh(
        f"for R in {roots}; do "
        f"[ -r \"$R\"/{name} ] && echo \"PRESENT $R\"; "
        f"done; true"
    )


def parse_counts(text):
    """(written, attempted) from the guest's own report; (0, 0) when absent."""
    m = _COUNT_RE.search(text or "")
    if not m:
        return 0, 0
    return int(m.group(1)), int(m.group(2))


def present_roots(text):
    """The roots a probe reported as readable."""
    return [line.split(" ", 1)[1].strip()
            for line in (text or "").splitlines()
            if line.startswith("PRESENT ")]


def summary_line(written, attempted):
    """One line for the boot log."""
    if attempted == 0:
        return ("execmark: the guest did not report — the menu will present "
                "itself as a generic device (floating button on screen)")
    if written == 0:
        return (f"execmark: NOT written (0/{attempted} roots) — the menu will "
                f"present itself as a generic device")
    return (f"execmark: {MARKER_NAME} written to {written}/{attempted} "
            f"candidate roots")
