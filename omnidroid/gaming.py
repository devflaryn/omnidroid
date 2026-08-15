"""Gaming-mode runtime tune-up — the mirror image of farming.py.

Same base, same adb transport, opposite objective. farming.py spends quality
and responsiveness to fit fifty instances in a host; this module spends memory
and CPU to make ONE instance feel like a game. Like farming.py it only BUILDS
the command sequence (pure, unit-testable); the engine applies it on a gaming
boot.

The important thing this module does is UNDO, not merely skip. Several farming
levers persist in the guest's /data, so an account that was farmed and is then
started in gaming mode keeps every one of them until something explicitly
reverses it:

    farming leaves                     gaming must restore
    ---------------------------------  --------------------------------------
    wm size 480x270 / density 80       wm size reset / wm density reset
    swappiness 100                     a low swappiness (frames vs zram)
    game in /dev/cpuset/background     game in top-app
    deviceidle force-idle              deviceidle disable
    IME disabled                       IME enabled (chat, login fields)
    engine tick capped at 5 fps        the gaming ClientAppSettings profile

Only ephemeral instances escape this by construction (their /data writes are
discarded at power-off), and "the ephemeral flag happens to save us" is not a
property worth depending on.

WHAT IS AND IS NOT MEASURED. Every number in farming.py came off a live
instance. Nothing here has: the joined-in-place gaming run is still blocked on
a working bootstrap APK (docs/superpowers/runbooks/B2-spike.md). These steps
are chosen because each reverses a specific, documented farming lever or
removes a specific, documented source of latency — not because they were
timed. Measure before quoting any of it as a result.
"""

import shlex

from omnidroid import lean
# `sh` and GAME_PKG are farming's, and stay farming's on purpose: the adb-shell
# quoting trap they document (adb re-parses a joined argv, so an unquoted
# `a; b` silently runs fragments of itself) is a property of the transport,
# not of either mode. Re-implementing it here would mean two copies of a rule
# that was already learned the expensive way once.
from omnidroid.farming import sh, GAME_PKG


def su_sh(su, script):
    """One adb step running `script` as ROOT in the guest.

    Shaped like the engine's existing root calls: `adb shell "su 0 sh -c
    '<quoted script>'"`, one argument, quoted once, so adb's re-parse of the
    joined argv lands the whole script in root's shell rather than the first
    word of it."""
    # "" is a VALID root mode: on the x86 Bliss base adbd ITSELF runs as uid 0,
    # so there is no wrapper to prepend and `{su} 0 sh -c ...` would run the
    # literal command `0`. See engine.resolve_root_shell.
    if su == "":
        return ["shell", f"sh -c {shlex.quote(script)}"]
    return ["shell", f"{su} 0 sh -c {shlex.quote(script)}"]


def root_only_steps():
    """Human-readable names of the steps that need root, for honest reporting.

    MEASURED on a live gaming instance (2026-08-06): as uid shell,

        $ adb shell cat /proc/sys/vm/swappiness
        cat: /proc/sys/vm/swappiness: Permission denied

    so writing it without root is a silent no-op — the step runs, the trailing
    `; true` swallows the failure, and the boot reports success having changed
    nothing. Rather than emit steps that cannot work, an unrooted instance
    omits them and the caller says so out loud. Exactly the rule
    apply_roblox_settings already follows, and the failure mode farming.sh
    exists to document."""
    return ("swappiness (keep the game's pages resident)",
            "top-app cpuset (give the game the latency-critical scheduler set)")

# Anonymous pages are the game's working set. Farming sets swappiness to 100
# so cold pages compress into zram instead of staying resident, which is right
# when nothing is watching the frame time — a zram decompress on the critical
# path is a stall a player sees. 10 is Android's own "keep it resident"
# convention; 0 is avoided so the kernel keeps the option under real pressure
# rather than OOM-killing the game.
GAMING_SWAPPINESS = 10

# Re-enabled explicitly rather than reversing the whole trim list. Undoing all
# ~34 packages would cost ~34 adb round trips on every gaming boot to restore
# apps a single-game kiosk still never opens; the IME is the one whose absence
# actually breaks interactive use, because with it disabled no text field in
# the game (chat, login, search) can be typed into at all.
REENABLE_PACKAGES = ("com.android.inputmethod.latin",)


# The density that goes with a GL panel override. 160 dpi is Android's mdpi
# baseline — one dp is one pixel — which is what the software path's 1280x800
# panel already reports, so overriding the size without it would leave the UI
# laid out for the GL device's own 120 dpi and everything would be drawn a
# sixth too small.
GL_PANEL_DENSITY = 160
# The panel height 160 dpi is the right answer FOR. Above this, density has to
# rise with the panel or the UI keeps its pixel size while the screen grows,
# and Roblox's on-screen controls end up postage stamps in a corner.
GL_PANEL_BASE_HEIGHT = 800
# Android refuses absurd densities and a mis-set one is unrecoverable without
# another `wm density` round trip, so clamp to the range real devices use.
GL_DENSITY_MIN, GL_DENSITY_MAX = 120, 480


def density_for_panel(panel, base_height=GL_PANEL_BASE_HEIGHT,
                      base_density=GL_PANEL_DENSITY):
    """The dpi that keeps UI elements the same APPARENT size on `panel`.

    Pure and clamped; never raises. Density is what Android lays out in — a
    button declared 48dp is 48 physical pixels at 160 dpi and 96 at 320 — so
    holding density fixed while the panel grows is what makes a 1440p guest
    look like a 800p one viewed from across the room. Scaling it with the
    panel's HEIGHT (not its area or its width) is what every Android device
    family does, and it keeps a 16:9 panel and a 16:10 one comparable.
    """
    try:
        height = int(panel[1])
    except (TypeError, IndexError, ValueError):
        return base_density
    scaled = round(base_density * height / float(base_height))
    # Land on a multiple of 20: Android buckets density into the ldpi/mdpi/…
    # families and an off-bucket value makes it pick assets by rounding anyway.
    scaled = int(round(scaled / 20.0) * 20)
    return max(GL_DENSITY_MIN, min(GL_DENSITY_MAX, scaled))


def build_tuning_sequence(mode=None, su=None, gl_panel=None):
    """Ordered list of adb `shell` argv vectors for the gaming tune-up.

    `mode` is a resolved MODES entry; None falls back to the gaming defaults so
    a caller without one still gets a correct sequence. `su` is the guest's
    root shell prefix (engine.resolve_root_shell): "" when adbd is already
    uid 0, a path when there is an su binary, or None on an unrooted instance,
    in which case the two root-only steps are OMITTED rather than
    emitted-and-ignored — see root_only_steps().

    `gl_panel` is the (w, h) the virtio GPU was told to advertise on a GL boot,
    or None on the software path. It exists because of a real defect: the GL
    device offers the guest a mode list that STARTS at 640x480, Android takes
    the first entry, and a GPU-accelerated boot therefore came up at 640x480
    while the software one gave 1280x800 — measured 2026-08-15, `wm size`
    reporting `Physical size: 640x480` on a device passed xres=1280,yres=800.
    `wm size reset` cannot fix that, because 640x480 IS the physical size as
    far as the guest is concerned; only an explicit override moves it.

    Ordering mirrors farming's reasoning: the display goes FIRST so everything
    measured afterwards is measured against the real resolution, and the
    cpuset move goes LAST because it needs a pid that only exists once the
    game is actually running.
    """
    mode = mode or {}
    steps = []

    # 1) The panel. `reset` on the software path, because the base's own size
    #    is the right answer there and hardcoding one would silently pin every
    #    future base to today's panel. An explicit size ONLY when a GL boot
    #    handed us one, which is the case `reset` gets wrong (see gl_panel).
    if gl_panel:
        w, h = gl_panel
        steps.append(["shell", "wm", "size", f"{w}x{h}"])
        steps.append(["shell", "wm", "density",
                      str(density_for_panel(gl_panel))])
    else:
        steps.append(["shell", "wm", "size", "reset"])
        steps.append(["shell", "wm", "density", "reset"])
    #    A mode carrying an explicit display still wins (NATIVE_DISPLAY is
    #    None, so gaming normally adds nothing here).
    steps += lean.display_args(mode.get("display"))

    # 2) Animations off. This is the one lever farming and gaming agree on,
    #    for different reasons: farming wants the CPU back, gaming wants the
    #    hundreds of milliseconds of transition that sit between an input and
    #    the frame that acknowledges it.
    for scale in ("window_animation_scale", "transition_animation_scale",
                  "animator_duration_scale"):
        steps.append(["shell", "settings", "put", "global", scale, "0"])

    # 3) Keep the game's pages resident. See GAMING_SWAPPINESS. Root-only.
    if su is not None:
        steps.append(su_sh(su, f"echo {GAMING_SWAPPINESS} > "
                               "/proc/sys/vm/swappiness 2>/dev/null; true"))

    # 4) No doze. farming force-idles the device to quiesce background work;
    #    a foreground game being throttled by the idle controller is exactly
    #    the stutter this mode exists to avoid. Whitelisting the game as well
    #    keeps it safe if something re-enables the controller later.
    steps.append(sh(f"dumpsys deviceidle whitelist +{GAME_PKG} "
                    ">/dev/null 2>&1; "
                    "dumpsys deviceidle disable >/dev/null 2>&1; true"))

    # 5) Restore the packages whose absence breaks interactive use.
    for pkg in REENABLE_PACKAGES:
        steps.append(sh(f"pm enable --user 0 {pkg} >/dev/null 2>&1; true"))

    # The cpuset move is NOT here. See build_pin_game_step: at this point in
    # the boot the game does not exist yet, so a pidof would find nothing.
    return steps


# How long the pin step waits in-guest for the game to appear. The kiosk
# launches it asynchronously once the session broadcast lands, and a cold
# start of a ~130 MB app on one emulated core is not instant. Fire-and-forget
# after that: a game that took longer than this still runs, just unpinned.
PIN_WAIT_SECS = 30


def build_pin_game_step(su):
    """Move the running game onto the top-app cpuset, or None without root.

    A SEPARATE step from build_tuning_sequence, and the reason is a bug this
    replaces. MEASURED on a live gaming instance (2026-08-06): the tune-up
    runs immediately after boot, and the game only starts when the session is
    delivered to the kiosk AFTERWARDS —

        $ adb shell su 0 sh -c 'pidof com.roblox.client'
        (empty)

    so a pidof-based move inside the boot sequence can never find a pid. It
    ran, matched nothing, hit its trailing `; true` and reported success: the
    silent-no-op shape farming.sh exists to warn about. It now runs after the
    session lands, and WAITS for the pid rather than sampling once, because
    the kiosk's launch is asynchronous and a single sample races it.

    top-app rather than foreground: it is the cpuset Android's own scheduler
    treats as the latency-critical app, which is exactly the claim this mode
    makes. Root-only — /dev/cpuset/*/tasks is not writable by uid shell.

    `su is None`, NOT `not su`. "" is a VALID root mode — it means adbd itself
    runs as uid 0, which is exactly what the x86 Bliss base does — and a
    falsy test read it as "no root" and returned None. That is the whole of
    the long-standing `cpuset: SKIPPED - no root on this instance` on a guest
    whose `adb shell id -u` answers 0 (re-confirmed by hand 2026-08-15). The
    same trap is documented on su_sh above and was fixed there; this call site
    was missed, so the one step that needed it most never ran. Every gate on a
    root mode has to be an identity test against None.
    """
    if su is None:
        return None
    return su_sh(su, (
        f"for i in $(seq 1 {PIN_WAIT_SECS}); do "
        f"PID=$(pidof {GAME_PKG} 2>/dev/null); "
        f'[ -n "$PID" ] && break; sleep 1; done; '
        f'[ -n "$PID" ] && echo $PID > /dev/cpuset/top-app/tasks '
        f"2>/dev/null; true"))
