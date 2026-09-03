"""Keep the NVIDIA GPU at full clocks while a guest is rendering.

THE PROBLEM THIS CLOSES. A virgl guest hands the host GPU a stream of small
GL batches with waits in between (occlusion-query readbacks, the swap), so
the host card never sees a load it considers heavy. NVIDIA's default power
mode ("Optimal power") answers by parking the RTX 4060 in P3/P5 at
465-675 MHz -- a fifth of its 3105 MHz -- and every one of those waits gets
five times longer. Measured 2026-09-03 in Pet Simulator 99: the render
thread spent ~14 ms of a 28 ms frame waiting on the GPU.

THE FIX is the one the NVIDIA Control Panel offers per application, "Power
management mode: Prefer maximum performance", written through NVAPI's
driver-settings (DRS) interface for `qemu-system-x86_64.exe`. No
administrator rights are needed (`nvidia-smi -lgc` does), it applies to the
next QEMU process, and the card ran at P0 2475 MHz during play: fence
latency 10 -> 1-3 ms, PS99 42-48 -> 50-52 fps. Two other per-app settings
were measured and are NOT set: forcing vsync off (45 fps) and "threaded
optimization" (35-38 fps, main thread busier).

Windows + NVIDIA only, by construction: no `nvapi64.dll` means no NVIDIA
driver, and the function returns a one-line reason instead of raising. On
Linux the equivalent is the `PowerMizerMode` / `nvidia-smi -lgc` route;
macOS has no NVIDIA. Never fails a launch.
"""
import ctypes
import os
import sys

PROFILE_NAME = "Omni Executor (QEMU)"
# NvApiDriverSettings.h
PREFERRED_PSTATE_ID = 0x1057EB71
PREFERRED_PSTATE_PREFER_MAX = 1
# nvapi_QueryInterface ids (stable across driver releases; nvapi.h)
_ID_INITIALIZE = 0x0150E828
_ID_DRS_CREATE_SESSION = 0x0694D52E
_ID_DRS_DESTROY_SESSION = 0xDAD9CFF8
_ID_DRS_LOAD_SETTINGS = 0x375DBD6B
_ID_DRS_SAVE_SETTINGS = 0xFCBC7E14
_ID_DRS_FIND_PROFILE_BY_NAME = 0x7E4A9A0B
_ID_DRS_CREATE_PROFILE = 0xCC176068
_ID_DRS_CREATE_APPLICATION = 0x4347A9DE
_ID_DRS_SET_SETTING = 0x577DD202
_ID_DRS_GET_SETTING = 0x73BF8338

_c = ctypes


class _Profile(_c.Structure):
    _fields_ = [("version", _c.c_uint32), ("profileName", _c.c_uint16 * 2048),
                ("gpuSupport", _c.c_uint32), ("isPredefined", _c.c_uint32),
                ("numOfApps", _c.c_uint32), ("numOfSettings", _c.c_uint32)]


class _ApplicationV1(_c.Structure):
    _fields_ = [("version", _c.c_uint32), ("isPredefined", _c.c_uint32),
                ("appName", _c.c_uint16 * 2048),
                ("userFriendlyName", _c.c_uint16 * 2048),
                ("launcher", _c.c_uint16 * 2048)]


class _Binary(_c.Structure):
    _fields_ = [("valueLength", _c.c_uint32), ("valueData", _c.c_uint8 * 4096)]


class _Value(_c.Union):
    _fields_ = [("u32", _c.c_uint32), ("binary", _Binary),
                ("wsz", _c.c_uint16 * 2048)]


class _Setting(_c.Structure):
    _fields_ = [("version", _c.c_uint32), ("settingName", _c.c_uint16 * 2048),
                ("settingId", _c.c_uint32), ("settingType", _c.c_uint32),
                ("settingLocation", _c.c_uint32),
                ("isCurrentPredefined", _c.c_uint32),
                ("isPredefinedValid", _c.c_uint32),
                ("predefined", _Value), ("current", _Value)]


def _ver(struct, v):
    return _c.sizeof(struct) | (v << 16)


def _wstr(arr, s):
    for i, ch in enumerate(s):
        arr[i] = ord(ch)
    arr[len(s)] = 0


def exe_name(exe_path):
    """The name the driver matches profiles on: the basename. Pure."""
    return os.path.basename(str(exe_path))


def describe(result):
    """One log line for `apply`'s result. Pure."""
    state, detail = result
    if state == "set":
        return ("nvidia: power mode 'prefer maximum performance' set for the "
                "QEMU exe (driver profile; the GPU stays at full clocks "
                "while the guest renders)")
    if state == "already":
        return "nvidia: power mode already 'prefer maximum performance' for QEMU"
    if state == "skipped":
        return f"nvidia: driver profile skipped ({detail})"
    return f"nvidia: driver profile NOT applied ({detail})"


def apply(exe_path):
    """Ensure the NVIDIA per-app profile for `exe_path` prefers max clocks.

    Returns (state, detail): "set", "already", "skipped" (no NVIDIA driver /
    not Windows), or "failed" (an NVAPI error code). Never raises."""
    if not sys.platform.startswith("win"):
        return "skipped", "not Windows"
    if os.environ.get("OMNI_NO_NVPROFILE"):
        return "skipped", "OMNI_NO_NVPROFILE set"
    try:
        nvapi = _c.WinDLL("nvapi64.dll")
    except OSError:
        return "skipped", "no nvapi64.dll (no NVIDIA driver)"
    try:
        qi = nvapi.nvapi_QueryInterface
        qi.restype = _c.c_void_p
        qi.argtypes = [_c.c_uint32]

        def fn(fid, *args):
            p = qi(fid)
            if not p:
                raise OSError(f"nvapi id {fid:#x} missing")
            return _c.WINFUNCTYPE(_c.c_uint32, *args)(p)

        vp = _c.c_void_p
        initialize = fn(_ID_INITIALIZE)
        create_session = fn(_ID_DRS_CREATE_SESSION, _c.POINTER(vp))
        destroy_session = fn(_ID_DRS_DESTROY_SESSION, vp)
        load = fn(_ID_DRS_LOAD_SETTINGS, vp)
        save = fn(_ID_DRS_SAVE_SETTINGS, vp)
        find_profile = fn(_ID_DRS_FIND_PROFILE_BY_NAME, vp, vp, _c.POINTER(vp))
        create_profile = fn(_ID_DRS_CREATE_PROFILE, vp, vp, _c.POINTER(vp))
        create_app = fn(_ID_DRS_CREATE_APPLICATION, vp, vp, vp)
        set_setting = fn(_ID_DRS_SET_SETTING, vp, vp, vp)
        get_setting = fn(_ID_DRS_GET_SETTING, vp, vp, _c.c_uint32, vp)

        r = initialize()
        if r:
            return "failed", f"NvAPI_Initialize {r}"
        sess = vp()
        r = create_session(_c.byref(sess))
        if r:
            return "failed", f"DRS_CreateSession {r}"
        try:
            r = load(sess)
            if r:
                return "failed", f"DRS_LoadSettings {r}"
            name = (_c.c_uint16 * 2048)()
            _wstr(name, PROFILE_NAME)
            prof = vp()
            if find_profile(sess, name, _c.byref(prof)):
                p = _Profile()
                p.version = _ver(_Profile, 1)
                _wstr(p.profileName, PROFILE_NAME)
                r = create_profile(sess, _c.byref(p), _c.byref(prof))
                if r:
                    return "failed", f"DRS_CreateProfile {r}"
                a = _ApplicationV1()
                a.version = _ver(_ApplicationV1, 1)
                _wstr(a.appName, exe_name(exe_path))
                _wstr(a.userFriendlyName, "Omni Executor guest")
                _wstr(a.launcher, "")
                r = create_app(sess, prof, _c.byref(a))
                if r:
                    return "failed", f"DRS_CreateApplication {r}"
            s = _Setting()
            s.version = _ver(_Setting, 1)
            if get_setting(sess, prof, PREFERRED_PSTATE_ID, _c.byref(s)) == 0 \
                    and s.current.u32 == PREFERRED_PSTATE_PREFER_MAX:
                return "already", ""
            s = _Setting()
            s.version = _ver(_Setting, 1)
            s.settingId = PREFERRED_PSTATE_ID
            s.settingType = 0            # NVDRS_DWORD_TYPE
            s.settingLocation = 0        # NVDRS_CURRENT_PROFILE_LOCATION
            s.current.u32 = PREFERRED_PSTATE_PREFER_MAX
            r = set_setting(sess, prof, _c.byref(s))
            if r:
                return "failed", f"DRS_SetSetting {r}"
            r = save(sess)
            if r:
                return "failed", f"DRS_SaveSettings {r}"
            return "set", ""
        finally:
            destroy_session(sess)
    except Exception as e:      # noqa: BLE001 -- never fail a launch for this
        return "failed", str(e)


__all__ = ["PROFILE_NAME", "PREFERRED_PSTATE_ID", "PREFERRED_PSTATE_PREFER_MAX",
           "exe_name", "describe", "apply"]
