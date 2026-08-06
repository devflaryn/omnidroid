"""Farming-mode runtime squeeze.

A joined-but-idle Roblox instance is pushed as small as stable AFTER boot, over
adb, without a separate image. This module only BUILDS the command sequence
(pure, unit-testable); the engine applies it on a farming-mode boot.

Division of labour with lean.py: anything that has to be a `ro.*` property, or
that means deleting files, is a BASE-IMAGE change and lives in lean.py behind
`omni strip-base`. Everything here is what can still be done to an already-
booted guest over adb. The split is not stylistic — `setprop ro.config.low_ram
true` at runtime is silently ignored, because init freezes `ro.*` once it has
set it. Trying to do the base-image tier from here is the obvious wrong turn.

Levers, in the order they are applied (the order is load-bearing — see
build_squeeze_sequence):
  - shrink the display, so every graphics buffer downstream shrinks with it;
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
    if not su:
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


def build_squeeze_sequence(mode=None):
    """Ordered list of adb `shell` argv vectors for the farming squeeze.

    `mode` is a resolved MODES entry; None falls back to the farming defaults
    so existing callers keep working.

    Ordering is load-bearing:
      1. display FIRST — every later step's memory picture is then measured
         against the small display, and shrinking it after apps have already
         allocated tablet-sized buffers just leaves the big buffers around;
      2. package disable before zram, so the pages freed by force-stopping
         those apps are free pages rather than things zram has to compress;
      3. cpuset LAST, because it needs the game's pid, which only exists once
         the game is actually up.
    """
    mode = mode or {}
    display = mode.get("display", lean.FARMING_DISPLAY)
    mem_mb = mode.get("mem", 2048)
    zram_mb = zram_size_mb(mem_mb)

    steps = []

    # 1) Shrink the display. Roblox keeps running and keeps its connection;
    #    it just composites ~30x fewer pixels.
    steps += lean.display_args(display)

    # 2) Tier-2 package trim. force-stop after disable so an already-running
    #    instance releases its pages now rather than at the next lmkd sweep.
    #    One shell per package on purpose: a single mega-command that failed
    #    halfway would silently skip every package after the failure.
    for pkg in lean.trim_packages():
        steps.append(sh(f"pm disable-user --user 0 {pkg} >/dev/null 2>&1; "
                        f"am force-stop {pkg} >/dev/null 2>&1; true"))

    # 3) zram swap on, so the guest reclaims under the low mem cap. lz4 over
    #    the zstd default: farming trades compression ratio for CPU, and CPU
    #    is the scarce resource when 50 instances share a host.
    steps.append(sh("swapon /dev/block/zram0 2>/dev/null || ("
                    "echo 1 > /sys/block/zram0/reset 2>/dev/null; "
                    "echo lz4 > /sys/block/zram0/comp_algorithm 2>/dev/null; "
                    f"echo {zram_mb}M > /sys/block/zram0/disksize 2>/dev/null || "
                    f"zramctl -f -s {zram_mb}M 2>/dev/null; "
                    "mkswap /dev/block/zram0 2>/dev/null; "
                    "swapon /dev/block/zram0 2>/dev/null); true"))

    # 4) Swap anonymous memory into zram aggressively. 100+ is the Android
    #    low-RAM convention: with a compressed backing device, swapping is
    #    cheaper than dropping the game's warm file pages.
    steps.append(sh("echo 100 > /proc/sys/vm/swappiness 2>/dev/null; "
                    "echo 0 > /proc/sys/vm/page-cluster 2>/dev/null; true"))

    # 5) lmkd: reclaim aggressively but keep the game alive. These are the
    #    runtime-settable half of lean.LMKD_PROPS (the ro.* half needs the
    #    baked base). Restarting lmkd makes it re-read them.
    steps.append(sh("setprop ro.lmk.use_psi true; "
                    "setprop ro.lmk.critical_upgrade true; "
                    "setprop ro.lmk.kill_heaviest_task true; "
                    "setprop ctl.restart lmkd; true"))

    # 6) Quiesce residual background work farming doesn't need. Safe on a
    #    headless kiosk instance.
    steps.append(["shell", "cmd", "activity", "idle-maintenance"])
    steps.append(["shell", "settings", "put", "global",
                  "window_animation_scale", "0"])
    # Doze the device, but whitelist the game FIRST — an un-whitelisted
    # Roblox would lose its network the moment doze engaged, which is the
    # exact opposite of "the instances must be on".
    steps.append(sh(f"dumpsys deviceidle whitelist +{GAME_PKG} "
                    ">/dev/null 2>&1; "
                    "dumpsys deviceidle force-idle >/dev/null 2>&1; true"))

    # 7) Ask the game to drop its own caches, then throttle it to the
    #    background cpuset. send-trim-memory is what the framework itself
    #    sends under pressure, so apps release exactly what they are built to.
    steps.append(sh(f"am send-trim-memory {GAME_PKG} RUNNING_CRITICAL "
                    ">/dev/null 2>&1; true"))
    steps.append(sh(f"PID=$(pidof {GAME_PKG} 2>/dev/null); "
                    f'[ -n "$PID" ] && echo $PID > '
                    f"/dev/cpuset/background/tasks 2>/dev/null; true"))
    return steps
