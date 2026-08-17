"""Farming-mode runtime squeeze.

A joined-but-idle Roblox instance is pushed as small as stable AFTER boot, over
adb, without a separate image. This module only BUILDS the command sequence
(pure, unit-testable); the engine applies it on a farming-mode boot.

Division of labour with lean.py: anything that has to be a `ro.*` property, or
that means deleting files, is a BASE-IMAGE change and lives in lean.py behind
`omnidroid strip-base`. Everything here is what can still be done to an already-
booted guest over adb. The split is not stylistic — `setprop ro.config.low_ram
true` at runtime is silently ignored, because init freezes `ro.*` once it has
set it. Trying to do the base-image tier from here is the obvious wrong turn.

Levers, in the order they are applied (the order is load-bearing — see
build_squeeze_sequence):
  - shrink the display, so every graphics buffer downstream shrinks with it;
  - drop to the RENDER FLOOR (`--quality minimal`): a smaller panel again, and
    the animation scales off, because the remaining cost of an unattended
    instance is the guest rasterising frames nobody looks at;
  - disable packages the single-game kiosk provably never uses;
  - enable zram swap so cold pages compress instead of staying resident;
  - tune lmkd (lowmemorykiller) to reclaim hard WITHOUT OOM-killing the game;
  - quiesce residual background work;
  - throttle the backgrounded game process to a background cpuset.
"""

import shlex

from omnidroid import lean

# The Roblox package the farming instance keeps joined-idle.
GAME_PKG = "com.roblox.client"


def sh(script):
    """One adb step running a multi-command shell script in the guest.

    The shlex.quote is NOT decoration, and removing it silently guts this
    module. `adb shell` does not forward argv — it joins the arguments with
    spaces and hands the result to the guest's shell to re-parse. So
    ["shell", "sh", "-c", "pm disable-user X; am force-stop X"] arrives as

        sh -c pm disable-user X; am force-stop X

    which the guest parses as `sh -c pm` (running bare `pm`, with the rest as
    positional parameters), then `am force-stop X` as a separate command. The
    intended first command never runs, and because every script here ends in
    `; true` the step still reports success.

    Measured 2026-08-05: with this quoting missing, the zram, swappiness,
    lmkd, doze, trim-memory and cpuset steps were all no-ops, and the package
    trim disabled nothing while its `am force-stop` half ran fine — which is
    exactly the failure that looks like it worked."""
    return ["shell", "sh", "-c", shlex.quote(script)]


# zram as a fraction of guest RAM. Android's own low-RAM guidance is ~50%;
# the compressed pool costs real RAM too, so more is not better.
ZRAM_FRACTION = 0.5


def zram_size_mb(guest_mem_mb):
    """zram device size for a guest with this much RAM, clamped to a sane
    band. Below ~128 MB the device is not worth its own metadata; above
    ~1 GB the compressed pool starts competing with the thing it exists to
    save."""
    return max(128, min(1024, int(guest_mem_mb * ZRAM_FRACTION)))


def build_client_settings_script(su, settings=None):
    """Shell script that installs Roblox's ClientAppSettings.json, or None.

    Returns None when `su` is None, and the caller MUST report that rather
    than pretending the step ran. The file lives inside the game's private
    data dir, so writing it needs root: `adb shell` runs as uid shell, which
    cannot enter /data/data/com.roblox.client, and `run-as` only works for a
    debuggable build. There is no non-root path on the production base — the
    honest options there are to bake the file into the base's /data image or
    to have the OmniBootstrap APK (which already injects the session cookie,
    and runs AS com.roblox.client) write it on startup.

    Ownership and SELinux label are restored explicitly: a file the app
    cannot read is the same as no file, except that it looks like it worked.

    And they are restored on files/ AS WELL AS on ClientSettings/, which is
    not belt-and-braces — it is the bug this docstring exists to prevent
    recurring. `mkdir -p .../files/ClientSettings` running as root creates the
    INTERMEDIATE files/ dir as root:root when it does not already exist, and
    chowning only the leaf leaves the app locked out of its own files/ dir.
    Measured on a live instance (2026-08-06):

        drwxr-xr-x 3 0     0     /data/data/com.roblox.client/files
        drwxr-xr-x 2 10138 10138 /data/data/com.roblox.client/files/ClientSettings

    with every other dir in the sandbox owned 10138. The game then cannot
    create anything under files/ —

        E SplitCompat:      Unable to create directory: .../files/splitcompat
        E CrossProcessLock: .../files/generatefid.lock: EACCES
        E FA:               .../files/google_app_measurement.db: EACCES

    — never finishes initialising, and drops out of the foreground. That is
    the "Roblox black-screens" symptom, caused by this installer rather than
    by the APK.

    Deliberately NOT a blanket `chown -R` over /data/data/<pkg>: cache/ and
    code_cache/ are owned 10138:20138 (a different GROUP), so recursing over
    the whole sandbox would corrupt them while fixing this.
    """
    # `is None`, not falsy: "" is a VALID root mode (adbd already runs as uid
    # 0 on the x86 base, so no wrapper is needed). Treating "" as "no root" is
    # what skipped this tune on every x86 launch — see engine.resolve_root_shell.
    if su is None:
        return None
    body = lean.client_settings_json(settings)
    files_dir = lean.CLIENT_SETTINGS_DIR.rsplit("/", 1)[0]
    return (
        f"mkdir -p {lean.CLIENT_SETTINGS_DIR}; "
        f"cat > {lean.CLIENT_SETTINGS_FILE} <<'OMNI_EOF'\n{body}\nOMNI_EOF\n"
        f"U=$(stat -c %u /data/data/{GAME_PKG}); "
        f"chown -R $U:$U {files_dir}; "
        f"chown -R $U:$U {lean.CLIENT_SETTINGS_DIR}; "
        f"restorecon -R {files_dir} 2>/dev/null; "
        f"restorecon -R {lean.CLIENT_SETTINGS_DIR} 2>/dev/null; true"
    )


# The squeeze's steps, by name, in the order they are applied. Every step
# carries its name so a caller can skip one BY NAME -- which is not a
# convenience: this sequence has now twice been the thing that stopped Roblox
# from running on the x86 base, and the only way to find out which lever did
# it is to run the guest with one of them removed. Bisecting it by editing
# this file means every attempt is a different build of the product.
STEP_DISPLAY = "display"
STEP_RENDER = "render"
STEP_TRIM_PACKAGES = "packages"
STEP_ZRAM = "zram"
STEP_SWAPPINESS = "swappiness"
STEP_LMKD = "lmkd"
STEP_QUIESCE = "quiesce"
STEP_DOZE = "doze"
STEP_TRIM_MEMORY = "trimmemory"
STEP_CPUSET = "cpuset"
STEP_NAMES = (STEP_DISPLAY, STEP_RENDER, STEP_TRIM_PACKAGES, STEP_ZRAM,
              STEP_SWAPPINESS, STEP_LMKD, STEP_QUIESCE, STEP_DOZE,
              STEP_TRIM_MEMORY, STEP_CPUSET)


def parse_skip(text):
    """Step names from a comma-separated list (OMNI_FARM_SKIP), ignoring
    anything that is not a real step -- a typo must not silently disable a
    different lever, and must not fail a boot either."""
    wanted = {p.strip().lower() for p in str(text or "").split(",") if p.strip()}
    return tuple(n for n in STEP_NAMES if n in wanted)


def build_display_sequence(mode=None, skip=(), quality=None):
    """The panel steps ALONE — `wm size` + `wm density`, or [] when skipped.

    Split out of build_squeeze_sequence, and the split is the fix for a bug
    the rest of that function cannot see. The squeeze runs AFTER the client
    has loaded (settle_density_instance), so emitting the panel from there
    delivers a display change to an already-running Roblox. Roblox's activity
    is `RESIZE_MODE_UNRESIZEABLE`, so Android cannot re-lay it out: it puts it
    in SIZE COMPAT MODE, scales the old window down and letterboxes it, and
    WM Shell parks "Tap to restart this app for a better view." on top of the
    game -- a prompt with a button, i.e. exactly what this product promises
    never to show.

    MEASURED 2026-08-17, PS99, x86 base (Android 13 / SDK 33). A farming boot,
    in-world, read back with `dumpsys activity activities`:

        resizeMode=RESIZE_MODE_UNRESIZEABLE
        mSizeCompatScale=0.5584416  mSizeCompatBounds=Rect(62, 0 - 419, 258)
        areBoundsLetterboxed=true   letterboxReason=SIZE_COMPAT_MODE

    i.e. a client that launched at 640x480/120dpi was still RENDERING 640x462
    and being downscaled into 357x258 of a 480x270 screen. Confirmed by
    INTERVENTION rather than inference: on a live GAMING instance with the
    game up and no prompt on screen, one `wm size 480x270` + `wm density 80`
    put the same prompt on screen within 20 s, same pid, no relaunch.

    Applied BEFORE the session is delivered instead, the client starts at the
    final panel, never enters size compat, and the prompt never appears --
    and it renders 480x270 rather than 640x462, which is 39% fewer pixels for
    free. This is the shape gaming has always had (its `wm size reset` runs in
    the boot tail, before the game exists); farming was the odd one out.

    `skip` still means what it says: OMNI_FARM_SKIP=display leaves the panel
    entirely alone, and `render` leaves it at the mode's own size.
    """
    mode = mode or {}
    skip = set(skip or ())
    if STEP_DISPLAY in skip:
        return []
    display = mode.get("display", lean.FARMING_DISPLAY)
    quality = quality if quality is not None else mode.get("quality")
    floor = lean.display_for_quality(quality, display)
    panel = display if STEP_RENDER in skip else floor
    return lean.display_args(panel)


def build_squeeze_sequence(mode=None, skip=(), quality=None):
    """Ordered list of adb `shell` argv vectors for the farming squeeze.

    `mode` is a resolved MODES entry; None falls back to the farming defaults
    so existing callers keep working. `skip` is a collection of STEP_NAMES to
    leave out (see the constants above, and OMNI_FARM_SKIP). `quality` is the
    EFFECTIVE quality profile for this boot — the engine resolves `--quality`
    against the mode's own default and knows the answer, and passing it in
    keeps that resolution in one place; None falls back to the mode's own
    `quality` key so a caller that does not care still gets the right thing.

    THE PANEL IS NOT HERE ANY MORE. It is build_display_sequence, applied
    before the session is delivered, because a `wm size` handed to a running
    non-resizable client is what raises Android's "Tap to restart this app for
    a better view" prompt — see that function for the measurement. What is
    left here is everything that is genuinely a property of an IDLE joined
    instance and so has to wait for the load to finish.

    Ordering is load-bearing:
      1. the RENDER FLOOR's animation scales first, so the steps that follow
         are measured against a guest that is no longer animating;
      2. package disable before zram, so the pages freed by force-stopping
         those apps are free pages rather than things zram has to compress;
      3. cpuset LAST, because it needs the game's pid, which only exists once
         the game is actually up.
    """
    mode = mode or {}
    skip = set(skip or ())
    mem_mb = mode.get("mem", 2048)
    zram_mb = zram_size_mb(mem_mb)

    steps = []

    # 1b) The RENDER FLOOR. Farming is unattended: no fps requirement, no view
    #     quality requirement, and — because the mode boots `-display none`
    #     with an idle VNC server that encodes nothing — no HOST-side render
    #     cost to attack. Every remaining lever is inside the guest, and what
    #     is left there is the guest rasterising frames nobody looks at.
    #
    #     A smaller panel again, but only when the quality profile actually
    #     asked for one: `display_for_quality` returns `display` unchanged for
    #     every profile except `minimal`, so a normal farming boot emits
    #     nothing here and its first step is still `wm size 480x270`.
    #
    #     Gated on the DISPLAY step as well as its own, and that is a bisect
    #     property rather than caution: `OMNI_FARM_SKIP=display` means "do not
    #     touch the panel", and a render floor that resized anyway would make
    #     that bisect prove nothing. To keep the panel at the mode's own size
    #     but drop the rest of the floor, skip `render` alone.
    #
    #     That costs a `minimal` boot a SECOND resize — 480x270 and then
    #     320x180 — and the transient is bought deliberately. Folding the floor
    #     into the display step would save two adb calls and destroy the
    #     property above: the two panels would stop being independently
    #     removable, which on the step this project has twice had to bisect is
    #     the wrong trade. The residual risk is that each `wm size` is a
    #     configuration change delivered to a running Roblox client; it already
    #     survives one (measured in-world on PS99, 2026-08-16), and surviving
    #     two is UNVERIFIED.
    #
    #     WHAT IS DELIBERATELY NOT HERE, because both look like obvious wins:
    #
    #       * setprop. Every compositing-related property in lean.py is `ro.*`
    #         — the HWUI cache tier, `ro.surface_flinger.max_frame_buffer_
    #         acquired_buffers`, `ro.config.avoid_gfx_accel`,
    #         `ro.zygote.disable_gl_preload`. init freezes `ro.*` once it has
    #         set it, so setting any of them from here is a SILENT no-op that
    #         reports success (lean.py's opening docstring is about exactly
    #         this split). The one non-`ro.` graphics key,
    #         `debug.sf.nobootanimation`, is read by SurfaceFlinger at boot and
    #         is long moot by the time this runs — the squeeze happens AFTER
    #         the client has loaded. And `ro.config.low_ram` is not merely
    #         inert here, it BRICKS the guest when baked (SystemUI crash-loop
    #         into recovery — lean.py:120-127). So this step emits no setprop
    #         at all; a placebo would be worse than nothing, because it would
    #         look like the lever had been pulled.
    #       * blanking the screen. `svc power`/a POWER keyevent would stop
    #         SurfaceFlinger compositing outright, and it is forbidden: a
    #         blanked farming instance stops rendering and stops EARNING,
    #         unnoticed. omnidroid/awake.py exists to prevent exactly this and
    #         runs on every boot in every mode.
    #
    #     What is left is real but modest, and is filed honestly as such:
    #     the panel, and the two animation scales the quiesce step does not
    #     already cover. Animations are pure raster work with no simulation
    #     effect, they run whenever a window or a view transitions, and a
    #     guest whose UI nobody watches has no use for a single frame of them.
    #     `window_animation_scale` is NOT repeated here — the quiesce step
    #     already sets it, and two steps writing the same setting would make a
    #     bisect of either one lie about what it changed.
    #
    #     NOT MEASURED. Every number in this module came off a live instance;
    #     this step has not been run against a place. It is chosen because each
    #     part removes a specific, named source of raster work, not because it
    #     was timed.
    if STEP_RENDER not in skip:
        steps.append(sh("settings put global transition_animation_scale 0 "
                        ">/dev/null 2>&1; "
                        "settings put global animator_duration_scale 0 "
                        ">/dev/null 2>&1; true"))

    # 2) Tier-2 package trim. force-stop after disable so an already-running
    #    instance releases its pages now rather than at the next lmkd sweep.
    #    One shell per package on purpose: a single mega-command that failed
    #    halfway would silently skip every package after the failure.
    if STEP_TRIM_PACKAGES not in skip:
        for pkg in lean.trim_packages():
            steps.append(sh(f"pm disable-user --user 0 {pkg} >/dev/null 2>&1; "
                            f"am force-stop {pkg} >/dev/null 2>&1; true"))

    # 3) zram swap on, so the guest reclaims under the low mem cap. lz4 over
    #    the zstd default: farming trades compression ratio for CPU, and CPU
    #    is the scarce resource when 50 instances share a host.
    #
    #    SKIPPED where the mode says so, and on x86 it does. Roblox is arm64
    #    only, so the x86 base runs it through libndk_translation, and swapping
    #    translated code pages out makes the translator take a SIGSEGV it
    #    cannot handle -- `Abort message: 'Cannot process signal 11'` in
    #    ndk_translation::HandleHostSignal, with memory free and no OOM kill.
    #    See MODES["farming"]["zram_x86"].
    if STEP_ZRAM in skip:
        pass
    elif not mode.get("zram", True):
        # NOT ENOUGH TO SKIP THE SWAPON: the base ships zram already ON.
        # `persist.sys.zram_enabled` is baked into build.prop and
        # /vendor/etc/init/zram.rc calls swapon_all at boot, so a guest that
        # was never asked to enable zram still comes up with ~1 GB of it --
        # measured, `SwapTotal: 1045168 kB` on an x86 farming instance whose
        # launch had just printed "zram: OFF for this mode". Turning it off
        # has to be an explicit step.
        steps.append(sh("swapoff /dev/block/zram0 2>/dev/null; "
                        "swapoff -a 2>/dev/null; true"))
    if mode.get("zram", True):
        steps.append(sh("swapon /dev/block/zram0 2>/dev/null || ("
                        "echo 1 > /sys/block/zram0/reset 2>/dev/null; "
                        "echo lz4 > /sys/block/zram0/comp_algorithm 2>/dev/null; "
                        f"echo {zram_mb}M > /sys/block/zram0/disksize 2>/dev/null || "
                        f"zramctl -f -s {zram_mb}M 2>/dev/null; "
                        "mkswap /dev/block/zram0 2>/dev/null; "
                        "swapon /dev/block/zram0 2>/dev/null); true"))

    # 4) How hard to swap anonymous memory. 100+ is the Android low-RAM
    #    convention and is right where there IS a compressed backing device and
    #    nothing minds its pages moving. On x86 the mode sets 10 instead: see
    #    the zram note above -- the translator cannot survive having its code
    #    pages evicted, and swappiness is the lever that decides how eagerly
    #    that happens.
    swappiness = mode.get("swappiness", 100)
    if STEP_SWAPPINESS not in skip:
        steps.append(sh(f"echo {int(swappiness)} > /proc/sys/vm/swappiness "
                        "2>/dev/null; "
                        "echo 0 > /proc/sys/vm/page-cluster 2>/dev/null; true"))

    # 5) lmkd: reclaim aggressively but keep the game alive. These are the
    #    runtime-settable half of lean.LMKD_PROPS (the ro.* half needs the
    #    baked base). Restarting lmkd makes it re-read them.
    if STEP_LMKD not in skip:
        steps.append(sh("setprop ro.lmk.use_psi true; "
                        "setprop ro.lmk.critical_upgrade true; "
                        "setprop ro.lmk.kill_heaviest_task true; "
                        "setprop ctl.restart lmkd; true"))

    # 6) Quiesce residual background work farming doesn't need. Safe on a
    #    headless kiosk instance.
    if STEP_QUIESCE not in skip:
        steps.append(["shell", "cmd", "activity", "idle-maintenance"])
        steps.append(["shell", "settings", "put", "global",
                      "window_animation_scale", "0"])
    # Doze the device, but whitelist the game FIRST — an un-whitelisted
    # Roblox would lose its network the moment doze engaged, which is the
    # exact opposite of "the instances must be on".
    if STEP_DOZE not in skip:
        steps.append(sh(f"dumpsys deviceidle whitelist +{GAME_PKG} "
                        ">/dev/null 2>&1; "
                        "dumpsys deviceidle force-idle >/dev/null 2>&1; true"))

    # 7) Ask the game to drop its own caches, then throttle it to the
    #    background cpuset. send-trim-memory is what the framework itself
    #    sends under pressure, so apps release exactly what they are built to.
    if STEP_TRIM_MEMORY not in skip:
        steps.append(sh(f"am send-trim-memory {GAME_PKG} RUNNING_CRITICAL "
                        ">/dev/null 2>&1; true"))
    if STEP_CPUSET not in skip:
        steps.append(sh(f"PID=$(pidof {GAME_PKG} 2>/dev/null); "
                        f'[ -n "$PID" ] && echo $PID > '
                        f"/dev/cpuset/background/tasks 2>/dev/null; true"))
    return steps
