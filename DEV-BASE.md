# omnidroid dev/prod base split — `base-dev.qcow2`

The x86 line is split into two registered bases that live side by side in the
images dir. **Nothing about the production base changed** — same filename, same
config, same behavior. The dev base is a separate, opt-in image.

| Base tag | Disk | Shipped? | Contents |
|----------|------|----------|----------|
| `x86` (production) | `base_x86.qcow2` | **Yes** | Bliss OS + kiosk, exactly as before. Untouched. |
| `arm` (production) | `base_arm.qcow2` | **Yes** | LineageOS arm64 pair, as before. Untouched. |
| `dev` (dev/debug)  | `base-dev.qcow2` | **No**  | `base_x86` + a frida / root-hiding **devkit** baked into `/system`. Only `omni-agent` ever boots it. |

`current_base` stays `x86`. Building or registering the dev base never changes
it, so every shipped account keeps booting the production base. The dev base is
selected **only** explicitly: `omni create <name> --base dev` (or, from the
agent, `ensure_emulator_running(dev=true)` / `OMNI_USE_DEV_BASE=1`).

## Building it

```bash
omni build-dev-base                 # frida (pinned) + Magisk resetprop applet
omni build-dev-base --json          # machine-readable result
omni build-dev-base --frida-version 17.15.4 --frida-port 27142
omni build-dev-base --no-magisk     # skip the resetprop applet
```

It reuses the exact `rebuild-base` pipeline shape: boot a throwaway builder on
the **pristine** `base_x86` (never `current_base`), `adb root` + `mount -o
remount,rw /`, inject the devkit into `/system`, flatten the overlay into
`base-dev.qcow2` (+ copy the x86 kernel/initrd to `base-dev.kernel` /
`base-dev.initrd.img`), and register the `dev` tag. `base_x86` is only ever read
(overlay-backed), so the production base cannot be modified by this command.

The build is idempotent: re-running rebuilds `base-dev.qcow2` from a fresh
`base_x86` overlay. Bases are immutable once accounts reference them — to change
the devkit, rebuild rather than editing in place.

## What gets baked into `/system` (dev base only)

Source for the scripts is `devkit/` (see `devkit/README.md`); binaries are
fetched at build time.

| Path | What |
|------|------|
| `/system/bin/frida-server` | stock frida-server (x86_64), pinned version. |
| `/system/bin/frida-server-patched` | *optional* anti-detection build — drop one in and `omni-fridad` prefers it. |
| `/system/bin/omni-fridad` | start frida-server **hidden**: custom loopback port (not 27042) + randomized process name. |
| `/system/bin/omni-frida-stop` | stop the devkit frida-server. |
| `/system/bin/omni-hide` | best-effort hide root+frida from a target app. |
| `/system/bin/omni-magisk` | Magisk multicall binary, used only as `omni-magisk resetprop …`. |
| `/system/etc/init/omni-devkit.rc` | `omni_fridad` init service (DISABLED by default). |
| `/system/etc/omni-devkit/manifest.json` | versions, frida port, what was installed. |

## Root & hiding model — KernelSU, not full Magisk

The Bliss base is already rooted with **KernelSU**. Installing a full Magisk
(patched boot ramdisk + its own `su`) on top on Android-x86 is a kernel-level
conflict that routinely soft-bricks the image. So the dev base:

- keeps **KernelSU** as the root provider (the base already had it — that's why
  `adb root` and `mount -o remount,rw /` work), and
- borrows only Magisk's **`resetprop` applet** for the prop-spoofing you
  actually need to defeat build-tag / verified-boot root checks, plus KernelSU's
  own per-app denylist for the specific app under test.

That is the honest reading of "use magisk to hide root and frida": the hiding
tools ship, without the risky full-Magisk-on-KernelSU install.

### What the base already gets right (measured on a booted dev account)

- The classic prop-based root/tamper "tells" are **already clean** in this Bliss
  base: `ro.build.tags=release-keys`, `ro.boot.verifiedbootstate=green`,
  `ro.debuggable=0`. So a detector reading those sees a stock-looking device
  out of the box — `omni-hide`'s resetprop step is mostly a no-op here (it
  reports "already clean") and exists for props a future base doesn't pre-set.
- **Root is KernelSU (kernel-level)**, so `adb root` and app `su` work
  regardless of `ro.debuggable` — normalizing build props never costs you root.

### Honest residual signals

- SELinux on this Bliss base is **Permissive**, so frida-server runs without
  ptrace friction — but a target can read `getenforce` and treat Permissive as a
  tamper signal. (Forcing Enforcing risks breaking the guest; left as-is.)
- KernelSU still leaves its own artifacts (the `su` implementation, the manager
  package) that a determined detector can probe; `omni-hide` requests KernelSU's
  per-app umount/denylist for the target when a `ksud` control path exists.
- Stock frida-server still names worker threads `gmain` / `gum-js-loop` /
  `pool-frida`. `omni-fridad` hides the *process name* and *port* but not those
  thread names — drop a patched `frida-server-patched` in to close that gap.

## Using it

```bash
# omnidroid, directly:
omni create dbg --base dev            # a dev account (frida baked in)
omni start dbg --wait
omni adb dbg -- root
omni adb dbg -- shell omni-fridad     # frida-server up, hidden, custom port
omni adb dbg -- shell omni-hide com.target.app

# omni-agent:
ensure_emulator_running(dev=true)     # or export OMNI_USE_DEV_BASE=1
ensure_frida_server()                 # -> frida -H 127.0.0.1:<forwarded port>
hide_root_from_app("com.target.app")
```

The dev base is a superset of the production x86 base, so everything else
(install, kiosk launch, screenshots, logcat, capture) works identically.

## Always-on auto-screenshots (dev base only)

A dev instance **captures screenshots automatically the whole time it is up** —
you never toggle it on. The moment `omni start`/`omni resume` finishes booting a
dev account, the engine spawns a continuous recorder that observes the VNC
framebuffer and drops a keyframe on **every big on-screen change** — a
black→loading flip (even one shown for a few milliseconds) is always caught,
while a spinning loader stays below the change threshold and is ignored, so you
don't drown in near-duplicate frames. It runs for the instance's whole lifetime
and is stopped on `omni stop`/`omni remove`. This is **dev-base only**: the
recorder never starts on `x86`/`arm`.

Output goes to **`$OMNI_AUTOCAP_DIR`** when set (omni-agent points that at its
`/workspace/screenshots/auto`), else `accounts/<name>/autocap`.

```bash
omni create dbg --base dev
omni start dbg --wait          # <- recorder auto-starts here (prints "auto-screenshots ON")
omni autocap dbg --status      # {running:true, out_dir:...}
#   ... drive the app; frames appear live in the out dir ...
omni stop dbg                  # <- recorder is finalized + stopped with the instance
```

`omni autocap <name>` manages it explicitly when needed (all idempotent):

```bash
omni autocap dbg --ensure      # start it if not already running (never stacks two)
omni autocap dbg --status
omni autocap dbg --stop
omni autocap dbg --restart --out DIR   # repoint the feed
```

The underlying recorder is `omni capture <name> --auto` (also runnable directly
for a manual, explicitly-scoped feed; refuses non-dev bases with
`dev_base_required`). Properties:

- **Live**: `metadata.json` is rewritten atomically after every kept keyframe, so
  a reader (omni-agent) sees frames as they land; `metadata.running` flips to
  `false` once finalized (logcat + crash/exit verdict folded in).
- **Timestamped filenames**: `frame_<index>_t<elapsed>ms_+<gap-since-previous>ms_w<HHMMSS_mmm>.png`
  — the gap tells an *instant* transition (+40ms) from one that *took time*
  (+62665ms) straight off the filename.
- **High cap**: an always-on session keeps up to 5000 keyframes (vs 240 for a
  bounded `--duration` window).
- **Stops on**: `omni stop`/`omni autocap --stop`, a `STOP` file in the out dir,
  `--max-seconds`, SIGINT/SIGTERM, or the instance powering off (VNC closes).

From `omni-agent` it is fully automatic: `ensure_emulator_running(dev=True)`
points the recorder at `/workspace/screenshots/auto/`, and the agent just calls
**`read_auto_screenshots`** to read the accumulating feed — no start/stop.

## Detection test — Roblox 2.726 (Byfron/Hyperion), 2026-07-13

Validated against `com.roblox.client` v2.726.1142 (arm64-only, via libndk), one of
the most aggressive mobile anti-tamper stacks, on a fresh `dev` account:

- **Not flagged.** Roblox boots to its normal login screen (Create Account /
  Sign In) — it does not refuse to boot and does not black-screen, with
  KernelSU root + the frida-server binary + `omni-magisk` all present.
- **Hidden frida-server undetected.** With `omni-fridad` running frida on the
  custom port, a naive scan finds nothing: no `:27042`, no process named
  `frida-server`, no `/data/local/tmp/frida-server`. Roblox behaved identically
  to frida-off (same process lifecycle, no `frida`/`hyperion`/`byfron`/`tamper`
  lines in `logcat -b all`).
- **Attach + hook works.** Host frida (17.15.4) connected through the forwarded
  hidden port, enumerated the guest (91 procs), **attached to Roblox and ran an
  in-process script** (`Process.enumerateModules()` → 321 modules, arch x64) —
  Roblox stayed alive and did not self-terminate.
- **An ANR ("Roblox isn't responding") on a warm force-stop+relaunch is emulator
  performance, NOT detection** — it reproduces identically with frida OFF (it's a
  5s input-dispatch timeout on the heavy `ActivitySplash` re-init). A cold launch
  (the agent's `run_apk_test_session` path) reaches the responsive login cleanly.

### Honest limits of this result

- Tested at the **login/splash** stage (no account, not inside an experience).
  Byfron/Hyperion's deepest checks engage when joining a game; that frontier was
  not exercised.
- frida attaches to the **x86_64 native** app process; the arm64 game code runs
  under libndk translation, so hooking translated arm64 frames is more involved
  than hooking native x86_64.

### Hardening ladder (if a tougher target ever does flag it)

1. **Perf:** cold-launch + a higher mode (`--mode playable`, more `--mem`) to
   avoid warm-relaunch ANRs — not a detection fix, just stability.
2. **Root:** the base already presents clean props (release-keys / green /
   `ro.debuggable=0`) and hides via KernelSU. For per-app invisibility against a
   determined detector, add **ZygiskNext** (Zygisk for KernelSU) + **Shamiko**
   (denylist) as KernelSU modules and denylist the target package.
3. **Active-frida detection** (in-process gum/gmain thread names): needs a
   patched frida — drop a build at `/system/bin/frida-server-patched` and
   `omni-fridad` prefers it. Note the public patched forks (strongR-frida,
   Florida) ship **arm/arm64 only**, so for this x86_64 guest that binary has to
   be built from source; stock frida (used here) was not detected at login.
