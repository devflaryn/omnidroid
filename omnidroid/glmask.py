"""Hide the renderer string a software/virtio guest reports to the game.

The problem
-----------
An instance that renders on llvmpipe answers `glGetString(GL_RENDERER)` with
`llvmpipe (LLVM 15.0.7, 256 bits)`, and one rendering through virglrenderer
answers `virgl (NVIDIA GeForce RTX 4060/PCIe/SSE2)`. Neither string exists on
any phone, and both are one call away from any app that cares. On the x86 base
that is measured, not theoretical:

    $ adb shell dumpsys SurfaceFlinger | grep GLES:
    GLES: Mesa, llvmpipe, OpenGL ES 3.2 Mesa 23.1.9

The lever
---------
Mesa reads four environment variables when it answers those queries --
`MESA_GL_RENDERER_OVERRIDE`, `MESA_GL_VENDOR_OVERRIDE`,
`MESA_GL_VERSION_OVERRIDE`, `MESA_GLES_VERSION_OVERRIDE` -- so the fix is to
put the first two into the GAME's environment. Nothing else on the device
needs to lie; SurfaceFlinger's own string is not something an app can read.

Getting an environment variable into one Android app's process is what the
`wrap.<process>` property is for. Zygote reads it at fork time
(`Zygote.applyInvokeWithSystemProperty`) and execs the app through
`/system/bin/sh -c "exec <value> /system/bin/app_process ..."`, so a value of
`FOO=bar` is an ordinary shell assignment applied to the app and to nothing
else.

Three real constraints, all of which this module enforces rather than hopes
for:

* **A property value is at most 91 characters** (`PROP_VALUE_MAX` is 92 with
  the NUL). Two overrides with quoted values fit; a third does not, which is
  why the version overrides are available but off by default.
* **`wrap.` is refused for a non-debuggable app on a user build.**
  `Zygote.applyInvokeWithSecurityPolicy` throws unless the process has
  `DEBUG_ENABLE_JDWP`, which every app gets when `ro.debuggable=1`. The x86
  base is a `userdebug` build (`...:userdebug/test-keys` in every tombstone),
  so it qualifies -- but that is a property of the image, so `verify()` reads
  the game's own `/proc/<pid>/environ` rather than trusting any of this.
* **It only takes effect at process START.** Setting it under a running client
  changes nothing until the client is restarted, which is exactly what
  `deliver_session` does when it hands over the session -- so the property has
  to be set BEFORE delivery, not after.

⚠ IT DOES NOT WORK ON THE x86 BASE, AND THE REASON IS FINAL
-----------------------------------------------------------
MEASURED 2026-08-15 on a live PS99 instance. With the property set, the client
**dies three seconds into startup**:

    Cmdline: com.roblox.client
    signal 31 (SIGSYS), code 1 (SYS_SECCOMP)
    Cause: seccomp prevented call to disallowed x86_64 system call 165
      #00 libc.so (mount+10)
      #01 libnativebridge.so (PreInitializeNativeBridge+1204)
      #02 libart.so (art::PreInitializeNativeBridge(...)+319)
      #03 libart.so (art::Runtime::Start()+6419)

Syscall 165 is `mount`. That call is the **arm64 translator setting itself
up**: `PreInitializeNativeBridge` bind-mounts the native-bridge library paths
into the app's mount namespace. On the ordinary fork path Zygote does that
before it installs the app's seccomp filter. The `invokeWith` path re-execs
through `/system/bin/sh`, so by the time the native bridge initialises the
filter is already on, `mount` is refused, and the process is killed.

Roblox ships arm64 only, so on the x86 base EVERY launch goes through that
native bridge -- which makes `wrap.` and arm64 translation mutually exclusive
there, not merely awkward. Clearing the property and relaunching brings the
client straight back. **So this is OFF by default.**

The two mechanisms that could still work, both image-side:

  * `export MESA_GL_RENDERER_OVERRIDE '...'` in the zygote's init rc, baked
    into the base. Zygote's environment is inherited by every forked app, so
    no process is wrapped and nothing re-execs -- the exact problem above
    cannot arise. Costs a base rebuild.
  * `setenv()` from OmniBootstrap, which already runs INSIDE the game process
    (it is what injects the session cookie). It has to land before the client
    creates its EGL context, which is later than it sounds. Costs an APK
    rebuild.

The arm base has no native bridge (Roblox runs natively there), so `wrap.` is
not expected to hit this at all -- untested, because the arm base is the Mac's
and the Mac renders in software anyway.

Honesty about what this does and does not buy
---------------------------------------------
It removes the `llvmpipe`/`virgl` tell. It ADDS a different one: an app that
reads its own environ can see `MESA_*_OVERRIDE`, which no real phone has. That
is a much less commonly checked signal than the renderer string, but it is not
nothing, and anyone reasoning about detection should know the trade rather
than discover it.
"""

import re

# What the guest claims to be. A mid-range Adreno is the safest default: it is
# the most common Android GPU family by a wide margin, and Roblox has run on
# it for years, so nothing about the string is unusual for a client to see.
DEFAULT_RENDERER = "Adreno (TM) 650"
DEFAULT_VENDOR = "Qualcomm"

# bionic's PROP_VALUE_MAX is 92 INCLUDING the terminating NUL, so 91 is the
# most that can be stored. A longer value is not truncated -- the set fails
# outright -- which would leave a boot silently unmasked.
PROP_VALUE_MAX = 91

GAME_PKG = "com.roblox.client"

# Mesa reads these; the names are Mesa's, not ours.
ENV_RENDERER = "MESA_GL_RENDERER_OVERRIDE"
ENV_VENDOR = "MESA_GL_VENDOR_OVERRIDE"

# The renderer strings that give the game away, for reporting what was found.
TELLS = ("llvmpipe", "virgl", "swiftshader", "softpipe", "SwiftShader",
         "ANGLE", "Mesa")


def wrap_prop(pkg=GAME_PKG):
    """The Zygote property that wraps this package's process launch."""
    return f"wrap.{pkg}"


def _shell_quote(value):
    """Single-quote for the guest shell that Zygote runs the wrap value in.

    Not `shlex.quote`: this string is embedded in a property whose whole value
    is later re-parsed by `sh -c`, so it must be quoted for THAT shell, and it
    must be quoted even when it looks safe -- `Adreno (TM) 650` contains
    parentheses, which are shell syntax."""
    return "'" + str(value).replace("'", "'\\''") + "'"


def wrap_value(renderer=DEFAULT_RENDERER, vendor=DEFAULT_VENDOR):
    """The `wrap.<pkg>` value that puts the overrides in the app's env."""
    parts = []
    if renderer:
        parts.append(f"{ENV_RENDERER}={_shell_quote(renderer)}")
    if vendor:
        parts.append(f"{ENV_VENDOR}={_shell_quote(vendor)}")
    return " ".join(parts)


def value_fits(value):
    return len(value) <= PROP_VALUE_MAX


def build_apply_script(value, pkg=GAME_PKG):
    """Root shell script that installs the wrap property and reads it back.

    Reads back on purpose: `setprop` exits 0 on a value the property service
    then refuses (over-long values, SELinux denial), so the only proof is the
    getprop. The caller compares."""
    prop = wrap_prop(pkg)
    # The echo is DOUBLE-quoted: the value carries spaces, and an unquoted
    # `$(getprop …)` would word-split them away, so a value that stuck would
    # read back subtly different from the one we asked for and the comparison
    # in the caller would report a failure that did not happen.
    return (f"setprop {prop} \"{value}\" 2>/dev/null; "
            f"echo \"WRAP:$(getprop {prop})\"")


def build_clear_script(pkg=GAME_PKG):
    prop = wrap_prop(pkg)
    return f"setprop {prop} \"\" 2>/dev/null; echo \"WRAP:$(getprop {prop})\""


def parse_applied(output):
    """The wrap value the guest actually holds, from build_apply_script's
    output, or None when the line is missing."""
    for line in reversed(str(output or "").splitlines()):
        line = line.strip()
        if line.startswith("WRAP:"):
            return line[len("WRAP:"):].strip()
    return None


def build_verify_script(pkg=GAME_PKG):
    """Read the LIVE game process's environment.

    `/proc/<pid>/environ` is NUL-separated; `tr` makes it greppable. This is
    the only check that proves the wrap actually applied -- the property
    existing proves only that we set a property."""
    return (f"P=$(pidof {pkg} 2>/dev/null | tr ' ' '\\n' | head -1); "
            f'[ -z "$P" ] && {{ echo ENV:NO_PID; exit 0; }}; '
            f"tr '\\0' '\\n' < /proc/$P/environ | grep -c "
            f"'^{ENV_RENDERER}=' | sed 's/^/ENV:/'")


def parse_verify(output):
    """(applied, detail) from build_verify_script's output.

    `None` for applied means "could not tell" (no game process yet), which is
    deliberately distinct from False ("the game is running and does NOT have
    it") -- they need opposite responses."""
    for line in reversed(str(output or "").splitlines()):
        line = line.strip()
        if line == "ENV:NO_PID":
            return None, "the game was not running when we looked"
        if line.startswith("ENV:"):
            try:
                n = int(line[len("ENV:"):].strip())
            except ValueError:
                continue
            return (n > 0), f"{n} match(es) in the game's environ"
    return None, "no answer from the guest"


def renderer_from_dumpsys(text):
    """The GLES renderer line SurfaceFlinger reports, for the before/after."""
    for line in str(text or "").splitlines():
        if line.strip().startswith("GLES:"):
            return line.strip()
    return None


def looks_like_a_tell(renderer):
    """True when this renderer string is one no real phone would report."""
    if not renderer:
        return False
    return any(re.search(t, renderer, re.I) for t in TELLS)
