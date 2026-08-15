# omnidroid/consent.py
"""Unattended consent: an instance must never sit waiting for a human tap.

Two independent halves, both per-/data, both idempotent:

  GRANT     the permissions a launch would otherwise be tapped through —
            "full disk access" (MANAGE_EXTERNAL_STORAGE) above all, plus the
            storage / overlay / install app-ops and every DANGEROUS runtime
            permission each installed app declares.
  SILENCE   the framework's own error dialogs (`hide_error_dialogs`), so an
            ANR or a crash kills the app instead of parking a modal over the
            screen until somebody dismisses it.

Like farming.py and awake.py this module only BUILDS the guest command
sequence — pure, unit-testable, no adb, no subprocess. The engine applies it
(`apply_consent`, every boot) and `offset consent` bakes the same sequence
into an offset image so the state ships inside the image too.

MEASURED on a live arm64 instance, Android 16 / SDK 36, 2026-08-12:

  * `settings put global hide_error_dialogs 1` takes effect IMMEDIATELY — no
    configuration change, no framework restart. A/B'd by crashing a stock app
    (`am crash com.android.settings`) with the flag at 0 and at 1: at 0 the
    "Settings keeps stopping" dialog appears, at 1 the identical crash leaves
    the screen untouched. That is why nothing here touches `wm size`/`wm
    density` to force AMS to re-read the flag — the kick is unnecessary, and
    on a farming boot it would silently undo the 480x270 the mode just set.
  * NONE of this needs root. `appops set`, `pm grant` and `settings put` all
    work as uid shell, which holds MANAGE_APP_OPS_MODES and
    GRANT_RUNTIME_PERMISSIONS — so the policy applies on an unrooted
    deployment exactly as it does on the shipped rooted base. Being able to
    run it without su is also what lets the bake reuse the plain builder boot.
  * `pm grant` refuses every NON-runtime permission, one by one. Roblox
    declares 24 `android.permission.*` entries of which 6 are runtime
    (POST_NOTIFICATIONS, READ_EXTERNAL_STORAGE, CAMERA, RECORD_AUDIO, …); the
    other 18 refusals are normal/signature permissions that are granted at
    install or not grantable at all. They are COUNTED, never printed as
    failures — a refusal there is the expected outcome, not a problem.

WHAT THIS DELIBERATELY DOES NOT DO. Granting REQUEST_INSTALL_PACKAGES removes
the "…isn't allowed to install unknown apps" gate, but when an app then
actually installs an APK, PackageInstaller shows its own confirm screen
("Update this app?" / "Do you want to install this app?"). No setting
suppresses that one — it is a deliberate user-consent step in the platform,
and the only ways past it are a UI tap or a privileged installer. Automating
it was considered and declined; see the CHANGELOG entry.
"""
import re
import shlex

# `sh` is farming's, and stays farming's on purpose: the adb-shell quoting
# trap it documents (adb re-parses a joined argv, so an unquoted `a; b` runs a
# fragment of itself and still reports success) is a property of the
# transport, not of any one module.
from omnidroid.farming import sh


# The environment kill switch, mirroring OMNI_NO_WARM / awake's. Set it when
# you WANT the dialogs — reproducing a crash-loop by eye is the obvious case.
NO_CONSENT_ENV = "OMNI_NO_CONSENT"


# Settings-provider writes, as (namespace, key, value).
SETTINGS = (
    # The whole "never show ANR / 'keeps stopping'" half. Read live by
    # ActivityTaskManagerService's shouldShowDialogs gate — see the A/B above.
    ("global", "hide_error_dialogs", 1),
    # The legacy unknown-sources master switch. Already 1 on the shipped base;
    # written anyway so the policy is self-contained on a base that lacks it.
    ("secure", "install_non_market_apps", 1),
)


# App-ops set to `allow` for every package the policy covers. These are the
# permissions that are NOT `pm grant`-able because the platform routes them
# through a full-screen consent activity instead of the runtime dialog.
APP_OPS = (
    # "Full disk access" / "All files access" — the one being asked for here.
    "MANAGE_EXTERNAL_STORAGE",
    # "…isn't allowed to install unknown apps" (the screenshot's dialog).
    "REQUEST_INSTALL_PACKAGES",
    # Scoped-storage opt-out, so a legacy path write is not silently empty.
    "LEGACY_STORAGE",
    "READ_EXTERNAL_STORAGE",
    "WRITE_EXTERNAL_STORAGE",
    # A floating mod menu is a SYSTEM_ALERT_WINDOW; without this it draws
    # nothing and the failure looks like the executor not loading.
    #
    # MEASURED 2026-08-12: this one is a NO-OP for a package that does not
    # declare the permission — `appops set` exits 0 and the mode still reads
    # `default` immediately afterwards, because the platform keeps this op in
    # sync with the (undeclared, therefore ungrantable) permission. Roblox
    # does not declare it, so it stays default there and the app draws its
    # menu inside its own window instead. Kept in the list because a companion
    # / executor APK installed later DOES declare it, and then it applies.
    # It is deliberately NOT in the probe: a bake or a boot must never report
    # failure over an op that cannot apply to the package it was asked about.
    "SYSTEM_ALERT_WINDOW",
)


# Echoed by the guest script only after every step has run, so a caller can
# tell "the policy applied" from "the shell died halfway and said nothing".
# Shaped like bases.GAME_BAKE_OK, and deliberately not a substring of the
# counts line that precedes it.
CONSENT_OK = "OMNI_CONSENT_OK"


# Emitted as `OMNI_CONSENT_COUNTS <packages> <ops> <granted> <refused>`.
COUNTS_PREFIX = "OMNI_CONSENT_COUNTS"


_COUNTS_RE = re.compile(
    rf"{COUNTS_PREFIX}\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)")


def consent_enabled(env=None):
    """False when the kill switch is set to a truthy value."""
    val = (env or {}).get(NO_CONSENT_ENV, "")
    return str(val).strip().lower() not in ("1", "true", "yes", "on")


def _settings_script():
    """Both settings writes in ONE round trip.

    `>/dev/null 2>&1` per write and a trailing `true`: a locked-down base can
    refuse an individual namespace, and one refusal must not abort the rest of
    the script or fail the boot step."""
    parts = [f"settings put {ns} {key} {value}"
             for ns, key, value in SETTINGS]
    return sh("; ".join(f"{p} >/dev/null 2>&1" for p in parts) + "; true")


def _package_list_expr(game_pkg=None):
    """Shell expression listing the packages the policy covers.

    Third-party packages PLUS the game, because the game is installed as an
    UPDATED SYSTEM APP (`pm install -r -d` over the pre-installed one, see
    bases.build_game_bake_script) and an updated system app does not appear in
    `pm list packages -3`. Leaving the list at `-3` would therefore skip the
    one package the whole policy exists for."""
    listing = "pm list packages -3 | sed 's/^package://'"
    if not game_pkg:
        return f"$({listing})"
    # `printf` then dedupe: the game is normally absent from -3, but a
    # dev/adb-installed build IS third-party and would otherwise be done twice.
    return (f"$({{ {listing}; printf '%s\\n' {shlex.quote(game_pkg)}; }} "
            f"| sort -u)")


def _policy_script(game_pkg=None):
    """App-ops + runtime grants for every covered package, in ONE round trip.

    A per-package adb call would be ~100 ms each on top of a boot the warm
    cache exists to keep at ~3 s, and the loop is trivial in the guest shell.

    The grant list comes from each package's OWN manifest
    (`dumpsys package <pkg>` → requested permissions), not from a hardcoded
    list: an APK that asks for something this module never heard of still gets
    it, and an APK that asks for nothing costs nothing."""
    ops = " ".join(APP_OPS)
    return sh(
        f"G=0; R=0; N=0; O=0; "
        f"for P in {_package_list_expr(game_pkg)}; do "
        f"N=$((N+1)); "
        f"for OP in {ops}; do "
        f"appops set $P $OP allow >/dev/null 2>&1 && O=$((O+1)); done; "
        f"for PERM in $(dumpsys package $P 2>/dev/null | "
        f"sed -n '/requested permissions:/,/install permissions:/p' | "
        f"grep -oE 'android\\.permission\\.[A-Z_]+' | sort -u); do "
        f"if pm grant $P $PERM >/dev/null 2>&1; then G=$((G+1)); "
        f"else R=$((R+1)); fi; done; done; "
        f"echo {COUNTS_PREFIX} $N $O $G $R; echo {CONSENT_OK}")


def build_consent_sequence(game_pkg=None):
    """Ordered adb `shell` argv vectors applying the whole policy.

    Order is load-bearing: the settings write goes FIRST so the error dialogs
    are already silenced while the (longer) permission loop runs — a crash
    during the loop must not be the one that parks a modal on screen."""
    return [_settings_script(), _policy_script(game_pkg)]


def build_persist_script():
    """Force the RAM-held half of the policy out to disk.

    APPLYING the policy and PERSISTING it into an image are different jobs,
    and only two of the three halves can be persisted at all. MEASURED on a
    live builder (Android 16 / SDK 36, 2026-08-12):

      settings   PERSIST. The provider writes on its own; `hide_error_dialogs`
                 still read 1 on a later boot of the committed image with the
                 boot-time step disabled. This is the half a bake can claim.
      app-ops    DO NOT persist across a power-off, on either image, twice.
                 `appops write-settings` reports "Current settings written"
                 and the modes do survive an `appops read-settings` (which
                 discards RAM state and reloads from disk) — so they reach
                 disk WITHIN the boot — yet a later boot of the committed
                 image reads them back as `default`. On this build the modes
                 are owned by the permission APEX
                 (/data/misc_de/0/apexdata/com.android.permission/access.abx,
                 the file `write-settings` touches), which re-derives them at
                 boot rather than trusting the file.
      pm grants  DO NOT persist either, and cannot even be flushed: runtime
                 permissions live beside the ops in that same APEX
                 (runtime-permissions.xml), no shell verb writes them, the
                 file was unchanged 45 s after a grant, and the orderly
                 shutdown did not write it.

    So this script is still worth running — it is what gets the ops onto disk
    for the read-back that follows, which is how the bake verifies it applied
    the policy at all — but the BAKE claims only the settings half. The other
    two are re-applied on every launch by the boot-time step, which is where
    they have to live regardless.
    """
    return sh("appops write-settings >/dev/null 2>&1; sync; true")


def build_reload_script():
    """Discard the RAM app-op state and reload it from disk.

    The verification half of build_persist_script: an op that still reads
    `allow` after this round trip is on disk, not merely in memory. Without
    it a bake can only report what it SENT."""
    return sh("appops read-settings >/dev/null 2>&1; true")


def build_consent_probe(game_pkg):
    """A guest script printing the state a caller can VERIFY, not infer.

    Reads back the one setting and the one app-op that the whole feature is
    about, in the exact shapes parse_consent_state expects."""
    return sh(
        f"echo hide_error_dialogs=$(settings get global hide_error_dialogs); "
        f"echo full_disk=$(appops get {shlex.quote(game_pkg)} "
        f"MANAGE_EXTERNAL_STORAGE 2>/dev/null | head -1); "
        f"echo install_unknown=$(appops get {shlex.quote(game_pkg)} "
        f"REQUEST_INSTALL_PACKAGES 2>/dev/null | head -1)")


def parse_consent_state(text):
    """{'dialogs_hidden', 'full_disk', 'install_unknown'} from probe output.

    Never raises and never guesses: an unreadable/absent field is False, which
    reports as NOT applied rather than as silently fine."""
    text = text or ""

    def flag(key):
        m = re.search(rf"^{key}=(.*)$", text, re.M)
        return (m.group(1).strip() if m else "")

    return {
        "dialogs_hidden": flag("hide_error_dialogs") == "1",
        "full_disk": "allow" in flag("full_disk").lower(),
        "install_unknown": "allow" in flag("install_unknown").lower(),
    }


def parse_counts(text):
    """(packages, ops_set, granted, refused) from the guest's counts line, or
    None when the line is absent — i.e. when the script did not finish."""
    m = _COUNTS_RE.search(text or "")
    return tuple(int(g) for g in m.groups()) if m else None


def applied_ok(text):
    """True iff the guest echoed the success marker."""
    return CONSENT_OK in (text or "")


def baked_summary(counts, state):
    """What a BAKE may honestly claim — which is less than what it applied.

    Separate from summary_line() on purpose. The bake runs the same sequence
    and reads the same state back, but only ONE of the three halves survives
    into the image (see build_persist_script). A single shared summary would
    make the bake report app-ops it does not actually persist, which is the
    exact overclaim this function exists to stop."""
    if not state.get("dialogs_hidden"):
        return "consent: error dialogs NOT silenced — nothing worth committing"
    applied = []
    if state.get("full_disk"):
        applied.append("full disk access")
    if state.get("install_unknown"):
        applied.append("install-unknown")
    granted = counts[2] if counts else 0
    return ("consent: error dialogs off (image-resident). "
            + (f"{', '.join(applied)} and {granted} runtime permission(s) "
               f"verified applied but NOT image state on this Android — "
               f"the boot-time step re-applies them on every launch"
               if applied else
               "app-ops did not apply in the builder"))


def summary_line(counts, state):
    """The one line the engine prints. Says what LANDED, from the read-back —
    never 'applied' on the strength of having sent the commands."""
    got = []
    if state.get("full_disk"):
        got.append("full disk access")
    if state.get("install_unknown"):
        got.append("install-unknown")
    if state.get("dialogs_hidden"):
        got.append("error dialogs off")
    if not got:
        return "consent: NOTHING landed — dialogs and permissions unchanged"
    head = ", ".join(got)
    if counts:
        pkgs, _ops, granted, _refused = counts
        return (f"consent: {head} "
                f"({pkgs} package(s), {granted} runtime permission(s) granted)")
    return f"consent: {head}"
