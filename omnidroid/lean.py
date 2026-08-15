"""Low-RAM policy: everything a single-game instance does NOT need.

OmniDroid runs exactly one app (Roblox). Every other thing a general-purpose
Android carries — a launcher, a settings app, an IME, a media scanner, HWUI
caches sized for a real tablet, a zygote that preloads a GL driver — is pure
overhead on an instance whose whole job is to keep one game process alive and
connected. This module is the single place that says what gets cut.

Three tiers, applied at three different times:

  BAKED_PROPS   -> appended to the base image's build.prop, OFFLINE
                   (`omnidroid strip-base`). Only tier that can set `ro.*`
                   properties: they are read-only once init has set them, so
                   `setprop ro.config.low_ram true` at runtime is a silent
                   no-op. This is why the big Android-level lever HAS to be a
                   base-image change and cannot live in the runtime squeeze.
  STRIP_APPS    -> app directories deleted from the base image, OFFLINE.
  RUNTIME_*     -> `pm disable-user` / `wm size` / zram / lmkd steps applied
                   over adb after boot (see farming.build_squeeze_sequence).

Measured context that set the numbers below (arm64 base, LineageOS 23.2, HVF,
2026-08-05): a booted instance with NO game idles at ~850 MB guest-used, of
which system_server ~343 MB, SystemUI ~317 MB, Settings ~194 MB, zygote64
~203 MB, media provider ~152 MB, latin IME ~149 MB. Roblox itself is ~533 MB
resident once ActivityNativeMain is up. So the system, not the game, is the
majority of the footprint — which is what this module attacks.
"""

# ---------------------------------------------------------------- properties

# The AOSP low-RAM master switch. ActivityManager.isLowRamDevice() keys off
# this and the framework then: caps the cached-process pool, skips the
# recents thumbnail cache, disables the SystemUI heap-heavy paths, shrinks
# the launcher icon cache, and turns off background dexopt. Nothing else in
# this file comes close to it for value-per-byte-changed.
LOW_RAM_PROPS = {
    "ro.config.low_ram": "true",
    # Cap cached/background apps. A single-game kiosk never benefits from a
    # warm cache of apps it will never foreground again.
    "ro.sys.fw.bg_apps_limit": "2",
    "ro.config.max_starting_bg": "1",
    # Skip the GL driver preload in zygote. The instance is headless and the
    # only GL client is the game, which loads its own driver anyway.
    "ro.zygote.disable_gl_preload": "true",
    "ro.config.avoid_gfx_accel": "true",
    # Do not keep a pool of pre-forked app processes warm.
    "persist.device_config.runtime_native.usap_pool_enabled": "false",
    # No boot animation: it is pure CPU + a decoded-frame buffer on a screen
    # nobody is watching.
    "debug.sf.nobootanimation": "1",
    # SurfaceFlinger holds one fewer buffer in flight. Quality/latency cost is
    # irrelevant for a headless instance; the buffer is real RAM.
    "ro.surface_flinger.max_frame_buffer_acquired_buffers": "2",
    # iorap prefetching trades RAM for app-start latency — the wrong trade
    # when the app starts once and then runs for hours.
    "ro.iorapd.enable": "false",
    "persist.sys.purgeable_assets": "1",
}

# AOSP's own low-memory Dalvik/ART profile (the 512 MB-device tuning from
# build/target/product/go_defaults_common.mk, rounded up one step because the
# game is a genuinely large app). A smaller growth limit makes ART collect
# sooner instead of letting each app balloon to a tablet-sized heap.
DALVIK_LOW_RAM_PROPS = {
    "dalvik.vm.heapstartsize": "4m",
    "dalvik.vm.heapgrowthlimit": "96m",
    "dalvik.vm.heapsize": "256m",
    "dalvik.vm.heaptargetutilization": "0.9",
    "dalvik.vm.heapminfree": "512k",
    "dalvik.vm.heapmaxfree": "2m",
}

# lowmemorykiller: reclaim hard, but the game is the LAST thing that may die.
# use_psi + critical_upgrade make lmkd act on pressure stalls rather than
# waiting for free-page watermarks, which is what keeps a squeezed instance
# responsive instead of thrashing.
LMKD_PROPS = {
    "ro.lmk.use_psi": "true",
    "ro.lmk.critical_upgrade": "true",
    "ro.lmk.upgrade_pressure": "40",
    "ro.lmk.downgrade_pressure": "60",
    "ro.lmk.kill_heaviest_task": "true",
    "ro.lmk.psi_partial_stall_ms": "70",
    "ro.lmk.psi_complete_stall_ms": "700",
}

# HWUI render-thread caches, sized for the tiny display a farming instance
# runs at (see RUNTIME_DISPLAY). Defaults are sized for a real tablet and are
# allocated per-process with a UI thread.
HWUI_LOW_RAM_PROPS = {
    "ro.hwui.texture_cache_size": "8",
    "ro.hwui.layer_cache_size": "4",
    "ro.hwui.path_cache_size": "2",
    "ro.hwui.gradient_cache_size": "0.5",
    "ro.hwui.drop_shadow_cache_size": "1",
    "ro.hwui.texture_cache_flushrate": "0.4",
    "ro.hwui.text_small_cache_width": "512",
    "ro.hwui.text_small_cache_height": "256",
    "ro.hwui.text_large_cache_width": "1024",
    "ro.hwui.text_large_cache_height": "512",
}

# Never compile more than needed. Background dexopt on a 1-core farming
# instance is a multi-hundred-MB RAM spike for an app that is already
# installed and never updates.
DEXOPT_PROPS = {
    "pm.dexopt.install": "speed-profile",
    "pm.dexopt.bg-dexopt": "verify",
    "pm.dexopt.boot-after-ota": "verify",
    "pm.dexopt.inactive": "verify",
}


# ===========================================================================
# BISECTED 2026-08-05 on the rooted dev base, via a Magisk system.prop module
# (which injects ro.* at early boot, exactly as a baked build.prop would).
# The result is that this profile is not worth baking, for two separate
# reasons — and both were found by isolating properties, not by guessing:
#
#   * `ro.config.low_ram=true` ALONE BRICKS THE GUEST. Tested by itself, with
#     nothing else set: the instance boots, SystemUI crash-loops, and Android
#     drops to RECOVERY with "Can't load Android system ... Reason:
#     RescueParty,com.android.systemui". Reproduced on the full 37-property
#     set and again on the single property. A full (non-Go) LineageOS build's
#     SystemUI does not survive being told it is a low-RAM device.
#     This is the ONLY property here with a large expected memory win, and it
#     is unusable on this base.
#
#   * EVERYTHING SAFE SAVES NOTHING. The complementary arm — lmkd + dexopt +
#     iorapd + bg-app limits, 8 properties, no low_ram and no dalvik — boots
#     fine and stays stable (verified with getprop). Measured guest-used did
#     not drop: 1069 MB before, ~1170 MB after. No win, inside noise or worse.
#     That is the expected result on reflection: lmkd tuning changes WHEN
#     processes get killed under pressure, not steady-state usage; the dexopt
#     properties only matter during install/OTA; and bg_apps_limit is moot
#     once the package trim has already stopped those apps.
#
# So baking properties buys nothing measurable and risks the fleet. The gate
# below stays ON, and the reason is now "no benefit + one known brick",
# not "untested". The tiers are kept because the bisect is worth continuing
# (dalvik and the hwui tier were never isolated individually), but nothing
# here should be baked into a shared base on the strength of a good name.
#
# Why the gate is not just a comment: strip-base writes to a base image that
# every account's COW overlay is backed by. Enabling it by accident would not
# break one instance, it would break the whole fleet at once.
# ===========================================================================

PROFILE_VERIFIED = False

PROFILE_EVIDENCE = (
    "baking this profile is not worth it, measured 2026-08-05 by bisecting on "
    "the DEV base: ro.config.low_ram=true ALONE bricks the guest (SystemUI "
    "crash-loop -> recovery, 'RescueParty,com.android.systemui'), and it is "
    "the only property here with a real memory win; the complementary safe "
    "subset (lmkd+dexopt+iorapd+bg limits, 8 props) boots fine but saved "
    "nothing (1069 MB -> 1170 MB guest-used). dalvik and hwui were never "
    "isolated individually - bisect one property at a time on the DEV base "
    "with a Magisk system.prop module before trusting any of them."
)

# The subset proven to BOOT (arm B above). Kept as evidence of what has been
# cleared, not as a recommendation: it boots, and it saves nothing.
BOOT_VERIFIED_PROPS = {
    "ro.sys.fw.bg_apps_limit": "2",
    "ro.config.max_starting_bg": "1",
    "ro.lmk.use_psi": "true",
    "ro.lmk.critical_upgrade": "true",
    "ro.lmk.kill_heaviest_task": "true",
    "pm.dexopt.bg-dexopt": "verify",
    "pm.dexopt.inactive": "verify",
    "ro.iorapd.enable": "false",
}

# Isolated and proven to BRICK the arm64 base. Never bake this.
BOOT_BREAKING_PROPS = ("ro.config.low_ram",)


def baked_props(include_unverified=False):
    """Every `ro.*`/build.prop property the lean base would bake in.

    Returns {} unless `include_unverified` is set, because nothing in this
    profile has been shown to boot — see the block comment above for the two
    measurements. Callers that genuinely want the experimental set (a bisect
    harness, a test) pass the flag; `omnidroid strip-base` requires an explicit
    --force-unverified from a human.

    Ordered least- to most-specific so a later tier can override an earlier
    one; today no key collides, and the test suite asserts that stays true."""
    if not (include_unverified or PROFILE_VERIFIED):
        return {}
    out = {}
    for tier in (LOW_RAM_PROPS, DALVIK_LOW_RAM_PROPS, LMKD_PROPS,
                 HWUI_LOW_RAM_PROPS, DEXOPT_PROPS):
        out.update(tier)
    return out


# Marks the block this module owns inside build.prop. merge_build_prop strips
# it before re-adding, which is what makes a re-bake idempotent instead of
# appending a fresh header every time.
PROFILE_MARKER = "# --- omnidroid lean profile (omnidroid strip-base) ---"


def build_prop_lines(props=None):
    """The exact text appended to a base image's build.prop.

    A trailing newline is deliberate: build.prop is line-oriented and the
    file we append to may not end in one, which would silently glue our
    first key onto the image's last line."""
    props = baked_props() if props is None else props
    body = "".join(f"{k}={v}\n" for k, v in sorted(props.items()))
    return "\n" + PROFILE_MARKER + "\n" + body


# ------------------------------------------------------------------- display

# Farming instances render at a postage stamp. Roblox still runs, still
# connects, still ticks its game loop — it just composites ~30x fewer pixels,
# which shrinks every graphics buffer in the pipeline (app surface,
# SurfaceFlinger, virtio-gpu framebuffer) at once. `quality does not matter`
# is the stated requirement; this is the single change that cashes it in.
FARMING_DISPLAY = (480, 270, 80)      # width, height, dpi
NATIVE_DISPLAY = None                  # playable/dev: leave the base's own

# The RENDER FLOOR panel, used by `--quality minimal` (see MINIMAL_APP_SETTINGS
# and farming.STEP_RENDER). 320x180 is 57 600 pixels against 480x270's
# 129 600 — 2.25x fewer to rasterise, composite and scan out, on an instance
# whose display exists only because Android insists on having one.
#
# UNVERIFIED. FARMING_DISPLAY was measured in-world on PS99 (screenshot,
# 2026-08-16); this one has not been run against a place at all. It is a
# smaller number of the same kind, not a result.
#
# WHY THE DENSITY IS 60 AND NOT LOWER, which is the part that can break the
# product rather than just make it ugly. Android reports the panel to apps in
# dp, as `px * 160 / dpi`, so the LOGICAL screen barely moves here:
#
#     480x270 @ 80 dpi  ->  960x540 dp,  0.500 px per dp
#     320x180 @ 60 dpi  ->  853x480 dp,  0.375 px per dp
#
# i.e. a layout that fits today still has room, while each dp costs a quarter
# less. Push the density lower and the dp count climbs without bound against a
# shrinking pixel budget, which is where a layout runs out of pixels to draw
# into — and Roblox's own UI is the one thing in this guest we cannot inspect
# or fix. `wm density` below ~60 HAS NOT BEEN VERIFIED, and the reason to be
# careful is not aesthetics: from outside the guest a collapsed or unclickable
# UI is INDISTINGUISHABLE from a hung client. adb is up, the process is alive,
# PSS is flat, the squeeze reports success — exactly the profile of the
# splash-screen wedge that cost this project a week (see
# farming.build_squeeze_sequence and the settle probe).
MINIMAL_DISPLAY = (320, 180, 60)


def display_for_quality(quality, default=FARMING_DISPLAY):
    """The guest panel a quality profile asks for.

    Only `minimal` overrides; everything else gets `default`, which the
    farming squeeze passes as the resolved mode's own `display` so this
    function can never silently shrink a mode that did not ask for it. An
    unknown quality name falls back to `default` rather than raising: this is
    called from the post-boot squeeze, where a bad name must cost the boot its
    render floor and nothing else. (`app_settings_for` returns None for an
    unknown name instead, because installing the WRONG ClientAppSettings
    profile is a real error and a slightly-too-big panel is not.)

    `default is None` — NATIVE_DISPLAY, "leave the base's own resolution
    alone" — wins over `minimal`, and that is the same trap resolve_mode's
    `guest_display` sentinel documents: None is a MEANINGFUL value here, not
    an absent one. Somebody who asked for the native panel and the render
    floor together gets the panel they asked for; the rest of the floor still
    applies."""
    if quality == "minimal" and default is not None:
        return MINIMAL_DISPLAY
    return default


def display_args(display):
    """adb argv vectors that apply a (w, h, dpi) display override, or [] for
    None (leave the base's native resolution alone)."""
    if not display:
        return []
    w, h, dpi = display
    return [
        ["shell", "wm", "size", f"{w}x{h}"],
        ["shell", "wm", "density", str(dpi)],
    ]


# -------------------------------------------------------------------- zram

# Getting zram onto a NON-ROOTED production instance.
#
# zram is worth a third of the per-instance footprint (lz4 compresses 496 MB
# of guest pages into 167 MB, 2.97x measured, dropping the safe balloon cap
# from 1536 MB to 1024 MB).
#
# The base ALREADY HAS THE ENTIRE MECHANISM. Read off a live instance
# (2026-08-05) before writing any of this:
#
#   /vendor/etc/fstab.virtio:
#       /dev/block/zram0 none swap defaults zramsize=50%
#   /vendor/etc/init/zram.rc:
#       on early-init   -> modprobe zram.ko
#       on init         -> write /sys/block/zram0/comp_algorithm lz4
#       on property:persist.sys.zram_enabled=1 -> swapon_all
#
# So the device, the compressor, the fstab entry and the swapon are all
# shipped by LineageOS and wired together. zram is not missing from this
# image; it is switched OFF behind one property. Flipping it is the whole
# job, and `zramsize=50%` then scales the device with whatever RAM the guest
# actually has -- better than any fixed size this file could pick.
#
# VERIFIED end-to-end on the real base: `setprop persist.sys.zram_enabled 1`
# made init run swapon_all and SwapTotal went 0 -> 470980 kB immediately.
#
# It is a persist.* property, not ro.*, so it is settable at runtime -- but
# NOT by uid shell (SELinux denies it; measured "Failed to set property").
# Hence root at runtime, or bake it into build.prop for production.
#
# An earlier draft of this module added a second zram line to the fstab via
# debugfs surgery. That was wrong twice over: the fstab already had the
# entry, and the candidate paths it probed did not include the real one
# (/vendor/etc/fstab.virtio), so it would have failed outright. Deleted
# rather than left as a fallback -- reading the image beats guessing at it.
ZRAM_ENABLE_PROP = {"persist.sys.zram_enabled": "1"}


# ----------------------------------------------------------- client settings

# Roblox's own engine settings, written to its ClientSettings directory. This
# is the only lever that reaches INSIDE the game, and it is the one the user's
# "quality and speed do not matter, you can disable rendering" explicitly
# licenses.
#
# MEASURED 2026-08-05 (dev base + the real Roblox APK, rooted, login screen):
#   memory  680 MB -> 677 MB   -- no change, and not a surprise in hindsight:
#                                the game's footprint is engine code, assets
#                                and script state, not framebuffers.
#   host CPU 36% -> 18.8%      -- roughly HALVED.
#
# So this is a CPU optimization, not a memory one, and it is filed here
# honestly as such. It still matters a great deal for the farming target:
# 50 instances at 36% of a core each need ~18 cores just to idle, and at
# 18.8% they need ~9. For a fleet, CPU is as binding as RAM, and this is the
# single biggest CPU lever available.
#
# DFIntTaskSchedulerTargetFps is doing most of the work: it caps the whole
# engine tick rate. 5 fps is plenty for an instance whose job is to stay
# joined and keep ticking; the rest force the lowest render path.
CLIENT_APP_SETTINGS = {
    "DFIntTaskSchedulerTargetFps": 5,
    "DFIntDebugFRMQualityLevelOverride": 1,     # lowest graphics quality
    "FFlagDisablePostFx": True,
    "FIntRenderShadowIntensity": 0,
    "FIntDebugForceMSAASamples": 0,
    "DFIntMaxFrameBufferSize": 4,
    "FIntTerrainArraySliceSize": 4,
    "DFIntCSGLevelOfDetailSwitchingDistance": 0,
    "FIntRenderLocalLightUpdatesMax": 1,
    "FIntRenderLocalLightUpdatesMin": 1,
    "FFlagRenderNoLowFrmRateCheck": True,
    "DFFlagDisableDPIScale": True,
}

# The RENDER FLOOR profile — farming's `low`, with the one key that still has
# room in it taken down another step.
#
# WHY IT IS DERIVED AND NOT RETYPED: `minimal` must be "everything `low` does,
# and less". Copying the dict out would let the two drift the first time
# somebody edits CLIENT_APP_SETTINGS, and a `minimal` that is accidentally
# HEAVIER than `low` is the kind of inversion nothing downstream would notice
# — the engine just installs whichever dict it is handed.
#
# THE TICK TARGET IS THE ONLY KEY LOWERED, and that is a finding rather than
# laziness. Everything else in the farming profile is ALREADY at its floor:
#
#   DFIntDebugFRMQualityLevelOverride  1   the bottom of Roblox's own 1..21
#                                          scale; there is no 0
#   FFlagDisablePostFx                 on  post-processing already off
#   FIntRenderShadowIntensity          0
#   FIntDebugForceMSAASamples          0
#   DFIntCSGLevelOfDetailSwitchingDistance 0
#   FIntRenderLocalLightUpdatesMax/Min 1
#
# Three keys COULD hold a smaller number and deliberately do not:
# DFIntMaxFrameBufferSize (4), FIntTerrainArraySliceSize (4) and the local
# light update pair (1 -> 0). Nothing in this repo records what units any of
# them are in or what the engine does at 0, and a client that renders nothing
# looks exactly like a client that hung — see the MINIMAL_DISPLAY note.
# Guessing a smaller number for a key whose meaning is unverified is how you
# buy an unfalsifiable bug, so they keep the values the farming profile
# MEASURED.
#
# 3 fps rather than 5: the tick target caps the WHOLE engine loop, which is why
# it was worth 36% -> 18.8% host CPU per instance on its own (2026-08-05), and
# it is the only remaining lever with a monotonic story — fewer ticks is less
# of everything, including the raster work nobody is looking at.
#
# UNVERIFIED, AND WITH A NAMED RISK. 5 fps is measured in-world on PS99; 3 is
# not, and two things could go wrong that this file cannot see from outside:
# the scheduler may clamp values below some floor (in which case `minimal`
# quietly equals `low`, which is harmless), or the client may fall behind its
# own network heartbeat and be dropped with a 27x error (which is NOT harmless,
# and looks like a healthy idle instance until you read the client log —
# engine.probe_client_join is what reads it). `--quality low` is the way back
# to the measured setting; bisect there first if instances start dropping.
MINIMAL_APP_SETTINGS = dict(CLIENT_APP_SETTINGS,
                            DFIntTaskSchedulerTargetFps=3)

# The GAMING profile — the same lever pulled the other way.
#
# Farming caps the engine tick at 5 fps because nobody is watching. Gaming
# wants the cap to stop being the limiter at all, so the renderer is what
# decides the frame rate. 240 rather than 60: the target is a ceiling, and a
# ceiling set at the refresh rate quantises the tick against it.
#
# The render-cost keys stay aggressive even here, and that is a deliberate
# reading of the requirement rather than an oversight. The stated goal for
# this mode is "high fps with low input latency" — quality is not on the list.
# It matters more than usual because the guest has NO 3D acceleration on the
# primary host (the QEMU there is built without virglrenderer — see
# qemu_proc.default_display), so every pixel of post-processing, MSAA and
# shadow work is done in software on the CPU that also has to run the game.
# Cutting those is the largest fps lever available until a GL-capable QEMU
# and a virgl-capable guest driver exist.
#
# Quality level 3 rather than farming's 1: the floor makes the game visually
# unusable, and this mode has a human looking at it.
#
# NOT MEASURED. The farming numbers in this file were each taken off a live
# instance; these have not been, because the joined-in-place run is still
# blocked (see docs/superpowers/runbooks/B2-spike.md). Treat them as a
# starting point to measure from, not as a result.
GAMING_APP_SETTINGS = {
    "DFIntTaskSchedulerTargetFps": 240,
    "DFIntDebugFRMQualityLevelOverride": 3,
    "FFlagDisablePostFx": True,
    "FIntDebugForceMSAASamples": 0,
    "FIntRenderShadowIntensity": 0,
    "DFFlagDisableDPIScale": True,
}

# The HIGH-QUALITY profile — what `playable`/`gaming` install by default.
#
# The requirement changed, and this profile is the change: `playable` is now
# the mode both a human PLAYS in and an AI TESTS in, and testing against a
# deliberately ugly render is testing a different program. A UI regression, a
# missing texture, a shader artefact, a wrongly-lit model — none of those are
# visible at quality level 3 with post-processing off, so the screenshots an
# agent reasons about have to come off a client rendering roughly what a real
# player sees.
#
# What is turned UP relative to GAMING_APP_SETTINGS:
#   quality level 3 -> 10   the mid-band of Roblox's own 1..21 scale: real
#                           textures, real materials, real lighting.
#   post-FX          on     the single biggest visual difference; without it
#                           the game looks flat and unlit.
#   shadows          on     at a low intensity, not off.
#   DPI scale        on     (DFFlagDisableDPIScale False) so the UI is laid
#                           out at the panel's real density rather than 1:1
#                           pixels, which is what a phone actually shows.
#
# What stays OFF, and why that is not an inconsistency: MSAA. On the primary
# host QEMU has no virglrenderer, so the guest has NO 3D acceleration and every
# sample is resolved in software on the same CPU running the game (see
# qemu_proc.default_display). Multisampling is the one lever that multiplies
# that cost per pixel with almost nothing to show for it at this resolution, so
# it is the one quality key not raised. Everything else here buys visible
# fidelity; MSAA would only buy edge smoothing at several times the frame cost.
#
# The tick target stays at 240 — a ceiling, not a target — so the RENDERER
# decides the frame rate, not the scheduler. Raising quality lowers the frame
# rate the renderer can sustain; that is the trade this profile takes on
# purpose, and `--quality balanced` is how you take the other one.
#
# NOT MEASURED, same caveat as GAMING_APP_SETTINGS.
PLAYABLE_APP_SETTINGS = {
    "DFIntTaskSchedulerTargetFps": 240,
    "DFIntDebugFRMQualityLevelOverride": 10,
    "FFlagDisablePostFx": False,
    "FIntDebugForceMSAASamples": 0,
    "FIntRenderShadowIntensity": 1,
    "DFFlagDisableDPIScale": False,
}


# The profiles, by the name `--quality` takes. Kept as ONE mapping so the CLI,
# the mode table and the engine cannot disagree about what a quality name means
# — the failure this codebase has already had twice (a flag accepted by
# argparse and then overridden downstream). `omnidroid start --quality` takes
# its choices straight off this dict, so adding a key here adds the flag value.
#
# Ordered heaviest-last on purpose: it reads as a ladder, and `minimal` sits
# BELOW `low` rather than beside it. `low` is the measured farming profile and
# stays the default for the density mode; `minimal` is the opt-in floor for
# "as many instances as this host will hold", and it is unverified in-world.
QUALITY_PROFILES = {
    "minimal": MINIMAL_APP_SETTINGS,   # render floor: 3 fps, 320x180 panel
    "low": CLIENT_APP_SETTINGS,        # farming: 5 fps, lowest everything
    "balanced": GAMING_APP_SETTINGS,   # max fps, effects off
    "high": PLAYABLE_APP_SETTINGS,     # real render; playable/gaming default
}


def app_settings_for(quality):
    """The ClientAppSettings dict for a quality name, or None if unknown.

    None on purpose rather than a silent fallback: a caller that passed a
    quality this build does not know must say so, not quietly install the
    farming profile onto a gaming boot."""
    return QUALITY_PROFILES.get(quality)


# Where the Roblox client reads them from, inside its own private data dir.
CLIENT_SETTINGS_DIR = "/data/data/com.roblox.client/files/ClientSettings"
CLIENT_SETTINGS_FILE = CLIENT_SETTINGS_DIR + "/ClientAppSettings.json"


def client_settings_json(settings=None):
    """The exact JSON written to ClientAppSettings.json.

    Sorted so the file is byte-stable across runs — it lands inside an image
    or a diff often enough that churn would be noise."""
    import json as _json
    return _json.dumps(settings or CLIENT_APP_SETTINGS,
                       indent=2, sort_keys=True)


# --------------------------------------------------- per-game memory floor

# How much guest RAM a PLACE needs. Not a tuning constant — a property of the
# game, measured per place id, and the distinction is the whole point of this
# table.
#
# `MODES["farming"]["mem"]` is 2048 and that is the right DEFAULT: it boots
# Android plus a Roblox client on a light place with room to spare, and every
# balloon figure in FOOTPRINT.md was taken against it. It is simply wrong for
# a heavy place, and no single number can be right for both — the variable is
# the game's world, which this engine does not get a vote on.
#
# MEASURED 2026-08-16, Pet Simulator 99 (place 8737899170), x86 base, in-world,
# `dumpsys meminfo com.roblox.client`:
#
#   game PSS on the LOGIN screen (what FOOTPRINT.md measured)  ~500-680 MB
#   game PSS loading and holding the PS99 world                1018 -> 1528 MB
#
# At 2048 MB the client was OOM-KILLED THREE TIMES IN A ROW, under gaming
# tuning, with no squeeze and no balloon in the way — so it was memory, not the
# squeeze and not the translator. At 3072 the same launch reached the world and
# stayed there (screenshot-verified: PS99's live leaderboard, chat scrolling,
# its own teleport logic running). See CHANGELOG 2026-08-16 and FOOTPRINT.md.
#
# AN UNMEASURED PLACE GETS THE DEFAULT AND MAY OOM. That is deliberate: this
# table records measurements, and inventing a floor for a place nobody has run
# would make it a table of guesses that reads like a table of facts. The
# symptom to look for is `has died: fg TOP` / `mem-pressure-event` in logcat
# while the client is still loading. Measure the place you intend to farm
# before promising a fleet size for it.
#
# Keys are STRINGS because a place id arrives as one (argv, run.json, the app's
# settings) and is 10 digits today — comfortably inside an int, but nothing
# here needs it to be one, and normalising on the way in beats hoping every
# caller agrees on the type.
GUEST_MEM_FLOOR_DEFAULT_MB = 2048

GUEST_MEM_FLOOR_MB = {
    "8737899170": 3072,   # Pet Simulator 99 — measured, see above
}


def guest_mem_floor_mb(place_id, default=GUEST_MEM_FLOOR_DEFAULT_MB):
    """Minimum guest MB measured for this place, or `default`.

    `default` is what the CALLER already decided to use (the resolved mode's
    `mem`, or a `--mem` the user typed), so an unmeasured place is left exactly
    as it would have been and a measured one can only be raised, never lowered:
    a place with a 3072 floor started with `--mem 4096` keeps its 4096. The
    max() is the part that makes this safe to wire in unconditionally.

    None/blank place id -> `default`. The launch path can genuinely not know
    the place (no `--place`, the client lands on the home screen), and that has
    to mean "no measurement applies", not a crash on the boot path."""
    key = str(place_id or "").strip()
    if not key:
        return default
    floor = GUEST_MEM_FLOOR_MB.get(key)
    return max(default, floor) if floor else default


# ------------------------------------------------------------------ packages

# Tier 1: packages engine.TRIM_PACKAGES disables during FIRST-BOOT
# provisioning. Repeated here because provisioning never runs on an ephemeral
# instance — build_acct() hands back a handle with first_boot_done already
# True, so `provision_settings` -> `lockdown_and_trim` is dead code on the
# production path, and every one of these was found resident on a booted
# farming instance (deskclock 138 MB, lineageos.updater 125 MB, lineageparts
# 124 MB, permissioncontroller 138 MB by RSS). Disabling them from the
# squeeze instead was measured at -70 MB guest-used, with the game running.
#
# Kept as its own tuple rather than importing engine.TRIM_PACKAGES: lean.py
# must not import engine (engine imports lean), and the two lists answer
# different questions — that one is "what a fresh install disables once",
# this one is "what a farming instance must not be paying for right now".
PROVISION_TRIM_PACKAGES = (
    "com.google.android.googlequicksearchbox",  # Assistant/search
    "com.android.deskclock",
    "org.lineageos.updater",                  # OTA updater (persistent)
    "org.lineageos.lineageparts",
    "com.android.permissioncontroller",
    "org.omnirom.omnijaws",                   # weather service
    "com.farmerbb.taskbar",
    "io.chaldeaprjkt.gamespace",              # game overlay
    "player.phonograph.plus",
    "com.android.dialer",
    "com.android.contacts",
    "com.android.messaging",
    "com.android.printspooler",
    "com.android.touch.gestures",
    "com.google.android.projection.gearhead",
    "com.google.android.syncadapters.calendar",
    "com.android.imsserviceentitlement",
    "com.android.cellbroadcastreceiver.module",
)

# Tier 2 package trim: things a cookie-delivered, single-game kiosk provably
# never uses. The session arrives by broadcast, so nobody ever types; nothing
# browses files; no media is scanned. `pm disable-user` is per-/data and
# reversible, and every instance is ephemeral, so this is never a one-way
# door.
FARMING_TRIM_PACKAGES = (
    "com.android.inputmethod.latin",       # ~149 MB; nothing is ever typed
    "com.android.settings",                # ~194 MB resident at idle
    "com.android.providers.media.module",  # ~152 MB; no media to scan
    "com.android.documentsui",
    "com.android.gallery3d",
    "com.android.calendar",
    "com.android.providers.calendar",
    "com.android.bluetooth",               # no BT device is ever attached
    "com.android.nfc",
    "com.android.se",                      # secure element
    "com.android.emergency",
    "com.android.traceur",                 # system tracing UI
    "com.android.wallpaper.livepicker",
    "com.android.bookmarkprovider",
    "com.android.providers.userdictionary",
    "com.android.managedprovisioning",     # device owner is already set
)

# Deliberately NOT trimmed, and why — this list is the guard rail against a
# future "just disable everything" change that breaks the product:
#   com.android.systemui  — MEASURED, not assumed (2026-08-05): running
#                           `pm disable-user --user 0 com.android.systemui`
#                           on a healthy booted instance takes the whole
#                           guest down. adb goes offline permanently and the
#                           QEMU process collapses to ~1.6 MB RSS; the guest
#                           never comes back. It is the single largest
#                           non-game process (~305 MB RSS) and therefore the
#                           most tempting thing on this list, which is why
#                           the evidence is recorded here. Shrink it via
#                           ro.config.low_ram + the tiny display instead.
#   com.google.android.gms / com.android.vending
#                         — Play Integrity. The game may attest. Removing
#                           these is a game-breaking change, not a RAM one.
#   com.omni.kiosk        — is the product.
#   com.roblox.client     — is the point.
#   com.android.networkstack.* / com.android.phone
#                         — connectivity; the instance must stay online.
KEEP_ALWAYS = (
    "com.android.systemui",
    "com.google.android.gms",
    "com.android.vending",
    "com.omni.kiosk",
    "com.roblox.client",
)


def trim_packages(extra=()):
    """The full farming trim list (tier 1 + tier 2), KEEP_ALWAYS enforced.

    The filter is not decoration: it makes "add a package to the trim list"
    a safe edit, because the five packages that must survive cannot be
    removed by accident even if someone names one."""
    keep = set(KEEP_ALWAYS)
    seen, out = set(), []
    for pkg in (tuple(PROVISION_TRIM_PACKAGES) + tuple(FARMING_TRIM_PACKAGES)
                + tuple(extra)):
        if pkg in keep or pkg in seen:
            continue
        seen.add(pkg)
        out.append(pkg)
    return out


# --------------------------------------------------------------- image strip

# `omnidroid strip-base` bakes PROPERTIES ONLY. Deleting the app directories
# themselves was considered and deliberately dropped: `pm disable-user`
# already stops those packages from running or holding memory, so removing
# their APKs buys only the PackageManager parse time and PackageSetting for a
# handful of apps — while turning a reversible, per-/data, per-instance
# setting into an irreversible edit to a shared base image that every
# account's COW overlay is backed by. The risk/benefit does not survive
# contact with "the base is immutable once any account references it".
#
# Where build.prop lives varies by image layout (system-as-root vs a nested
# system/ directory), so the strip probes these in order and edits the first
# that exists rather than assuming one.
BUILD_PROP_CANDIDATES = (
    "/system/build.prop",
    "/build.prop",
    "/system/etc/prop.default",
    "/etc/prop.default",
)


def merge_build_prop(existing_text, props=None):
    """Return build.prop text with `props` applied.

    Keys we are setting are REMOVED from the original body first, then our
    block is appended. Appending alone would not work and the reason is
    Android-specific: init refuses to redefine a `ro.*` property once it has
    been set, so for read-only properties the FIRST definition in the file
    wins and a duplicate appended at the end is silently ignored. Every
    property that matters here (`ro.config.low_ram` above all) is read-only,
    so "append our values" is precisely the implementation that looks right
    and does nothing.

    Re-baking an already-baked image is idempotent: our own marker comment is
    stripped along with our keys, so a second run replaces the block rather
    than leaving an orphaned header behind and appending another. Without
    that, every rebuild of a base would add a dead header line — harmless
    individually, unbounded across a base's lifetime.
    """
    props = baked_props() if props is None else props
    keys = set(props)
    kept = []
    for line in (existing_text or "").splitlines():
        stripped = line.strip()
        if stripped == PROFILE_MARKER:
            continue
        # Other comments and blanks are preserved verbatim; only real
        # assignments are candidates for removal.
        if stripped and not stripped.startswith("#") and "=" in stripped:
            if stripped.split("=", 1)[0].strip() in keys:
                continue
        kept.append(line)
    body = "\n".join(kept).rstrip("\n")
    return body + "\n" + build_prop_lines(props)
