"""Whether this Mac can render the guest on the GPU, and what is missing.

Three prerequisites, none of them code in this repository, all of them
presenting to the user as the same symptom -- a black or 3 fps guest. So the
product names them rather than letting somebody rediscover them:

1. A virgl-capable QEMU. Homebrew core has no `virtio-gpu-gl-pci` at all
   (`-display help` there is none/curses/cocoa/dbus). The route that works is
   knazarov's libangle + libepoxy-angle + virglrenderer formulae with QEMU
   built against them into a PRIVATE PREFIX; `startergo/qemu-virgl-kosmickrisp`
   names its formula `qemu` and Homebrew evicts the working one.
2. An arm base rebuilt with `ro.hardware.egl=mesa`. The image ships `angle`,
   so guest GL goes ANGLE -> SwiftShader in software no matter what the host
   offers. Mesa (/vendor/lib64/egl/libEGL_mesa.so) and the render node
   (/sys/class/drm/renderD128) are already there; only the property is wrong,
   and ro.* is immutable after init, so setprop cannot fix it.
3. Xcode CLT new enough for Homebrew to build from source. Do NOT probe this
   with `brew install --dry-run` on a bottled formula: a bottle never invokes
   a compiler, so the probe passes on a machine that cannot build anything.

This module is pure. The caller supplies the two host facts.
"""
from omnidroid.qemu_proc import GL_GPU_DEVICE

MESA_MARKER = "mesa"
VIRGL_MARKER = "virgl"


def readiness(qemu_device_help="", surfaceflinger_gles=""):
    """{"ready": bool, "blockers": [str, ...]}. Never raises.

    An EMPTY SurfaceFlinger line is a blocker, not a pass: "we could not ask"
    and "the answer was good" are different states, and treating the first as
    the second is how a software guest gets reported as accelerated.
    """
    blockers = []
    if GL_GPU_DEVICE not in (qemu_device_help or ""):
        blockers.append(
            f"this QEMU has no {GL_GPU_DEVICE} (built without virglrenderer). "
            f"Build one into a private prefix and point config `qemu.dir` at "
            f"it; never replace the Homebrew `qemu` formula.")
    gles = (surfaceflinger_gles or "").lower()
    if not gles:
        blockers.append(
            "could not read the guest's GLES renderer "
            "(`adb shell dumpsys SurfaceFlinger | grep GLES:`), so whether it "
            "is accelerated is unknown")
    elif not (MESA_MARKER in gles and VIRGL_MARKER in gles):
        blockers.append(
            "the guest renders through ANGLE/SwiftShader in software: the arm "
            "base ships ro.hardware.egl=angle and ro.* is immutable after "
            "init, so the base must be REBUILT with ro.hardware.egl=mesa")
    return {"ready": not blockers, "blockers": blockers}
