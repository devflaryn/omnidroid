# omnidroid/config.py
"""Single source of truth for platform, paths, and QEMU-binary resolution.

Every path/arch decision in OmniDroid routes through here so one checkout runs
on Windows, macOS, and Linux without any other module knowing the branches.
"""
import os
import platform
import sys
from pathlib import Path


def _app_root():
    """Project root — works both as a .py and as a PyInstaller onefile exe.
    When frozen, files live next to the exe (sys.executable), not in the
    temporary _MEIPASS extraction dir."""
    if getattr(sys, "frozen", False):
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent.parent


REPO = _app_root()
# OMNIDROID_CONFIG_PATH lets an embedding host (e.g. the omni-executor GUI's
# frozen --omnidroid subprocess) point the loader at a config file it wrote
# itself, instead of the fixed REPO/configs/paths.json. This must be read at
# import time: engine.py and bases.py do `from .config import CONFIG_PATH`
# (a direct name binding evaluated once, at import), and the embedding host
# sets the env var BEFORE importing omnidroid, so resolving it here is
# correct and sufficient -- no other module recomputes REPO/configs itself.
_env_cfg = os.environ.get("OMNIDROID_CONFIG_PATH")
CONFIG_PATH = Path(_env_cfg) if _env_cfg else REPO / "configs" / "paths.json"
QEMU_DIR = REPO / "qemu"          # local (auto-installed) QEMU lives here
IS_WINDOWS = platform.system() == "Windows"
IS_LINUX = platform.system() == "Linux"
IS_MACOS = platform.system() == "Darwin"
# Host CPU architecture. arm64 Macs (Apple Silicon) run the arm64 base
# natively under HVF with NO translation layer; x86_64 hosts run the Bliss
# x86 base under WHPX/KVM. Base selection keys off this (see host_arch_base).
HOST_ARCH = platform.machine().lower()
IS_ARM64_HOST = HOST_ARCH in ("arm64", "aarch64")


def data_dir() -> Path:
    """Directory holding accounts.json, accounts/, logs/, runtime/.
    Defaults to the project root; OMNI_DATA_DIR relocates it (created if
    missing) so state can travel independently of the code checkout."""
    env = os.environ.get("OMNI_DATA_DIR")
    if env:
        p = Path(env).expanduser()
        p.mkdir(parents=True, exist_ok=True)
        return p
    return REPO


def runtime_root() -> Path:
    """Directory holding per-instance throwaway state (runtime/<username>/).
    Under the data dir so it follows OMNI_DATA_DIR; created on demand."""
    p = data_dir() / "runtime"
    p.mkdir(parents=True, exist_ok=True)
    return p


def resolve_images_dir(cfg):
    """images_dir may be a plain string or a per-platform dict
    ({"windows": ..., "linux": ...}) so one checkout works on both hosts.
    ~ is expanded; a relative path is resolved against the project root
    (default: images/ inside the checkout, travels with the repo)."""
    v = cfg["images_dir"]
    if isinstance(v, dict):
        # macOS has no dedicated key in older configs: it explicitly falls
        # back to the linux path convention (~/OmniImages-style), same as
        # the historical behavior.
        key = "windows" if IS_WINDOWS else "darwin" if IS_MACOS else "linux"
        v = v.get(key) or (v.get("linux") if IS_MACOS else None) \
            or v.get("default")
        if not v:
            sys.exit(f"error: configs/paths.json images_dir has no entry "
                     f"for platform '{key}'")
    p = Path(v).expanduser()
    if not p.is_absolute():
        p = REPO / p
    return str(p)


def images_dir(cfg) -> str:
    """Absolute images dir. OMNI_IMAGES_DIR wins over the config value so a
    host can point at an external image store without editing paths.json."""
    env = os.environ.get("OMNI_IMAGES_DIR")
    if env:
        return str(Path(env).expanduser())
    return resolve_images_dir(cfg)


# ---------- qemu resolution + auto-install ----------

def qemu_bin(tool):
    """Resolve a QEMU executable path from the PRODUCT directory only.
    Order: OMNI_QEMU_DIR (the embedding host's installed QEMU) -> config
    'qemu.dir' (an explicit product-side override) -> local QEMU_DIR
    (auto-downloaded, next to the engine). On Windows the shipped product
    NEVER falls back to a host/global/PATH QEMU: if it isn't in the product
    dir yet, the returned (non-existent) product path drives ensure_qemu() to
    download it there. On Linux/macOS the documented model is SYSTEM QEMU
    (apt/brew), so a bare name (PATH) is the final fallback.

    OMNI_QEMU_DIR exists because QEMU_DIR is the WRONG home for an installed
    QEMU under the desktop app: it resolves to <exe dir>/qemu, and the app's
    updater replaces that whole tree on every update (updates.py
    apply_staged_app renames the app dir aside and copies the new build in),
    which would delete a 200 MB install on each release. The executor installs
    it beside the images instead and points the engine here — env, not just
    config, so a subprocess still resolves it if paths.json is stale or was
    written by a different install."""
    exe = tool + (".exe" if IS_WINDOWS else "")
    env_dir = os.environ.get("OMNI_QEMU_DIR")
    try:
        from omnidroid import engine
        qd = engine.read_config().get("qemu", {}).get("dir")
    except Exception:
        # Deliberately broad: engine.read_config() can fail for many reasons
        # (no config yet, mid-import cycle since engine imports config, a
        # corrupt file, ...). Any failure here just means "no override" --
        # never a reason to blow up qemu_bin() resolution.
        qd = None
    for cand in ([Path(env_dir) / exe] if env_dir else []) \
            + ([Path(qd) / exe] if qd else []) + [QEMU_DIR / exe]:
        if cand.exists():
            return str(cand)
    if IS_WINDOWS:
        # Product-dir only — never PATH. ensure_qemu() populates QEMU_DIR.
        return str(QEMU_DIR / exe)
    return tool          # Linux/macOS: system QEMU (apt/brew) is the model


def qemu_system_name():
    """The QEMU system emulator this HOST needs: aarch64 on Apple Silicon
    (arm64 guest, native under HVF), x86_64 everywhere else."""
    return "qemu-system-aarch64" if (IS_MACOS and IS_ARM64_HOST) \
        else "qemu-system-x86_64"


def self_argv_prefix():
    """Argv the frozen HOST binary needs before an omnidroid subcommand.

    The engine re-invokes itself for detached children (the VNC viewer, the
    autocap recorder) as `[sys.executable, "<subcommand>", ...]`. That is
    correct for the standalone omnidroid.exe, whose frozen entry point IS the
    engine CLI. It is WRONG when the engine is embedded in a host app:
    omni-exec.exe's entry point is the GUI, which only routes to the engine
    when argv[1] is "--omnidroid". Without the prefix, `omni-exec.exe
    _vncview ...` falls through and launches a SECOND COPY OF THE GUI instead
    of the viewer -- which is exactly what clicking "Open viewer" did.

    The host declares its own shape via OMNIDROID_SELF_ARGV (omni-executor
    sets "--omnidroid"); unset means "my argv is the engine's", the
    standalone-exe behaviour, so nothing changes there.
    """
    import shlex
    return shlex.split(os.environ.get("OMNIDROID_SELF_ARGV", ""))


# ---------- Windows console suppression (GUI embedding) ----------

CREATE_NO_WINDOW = 0x08000000


def _has_own_console():
    """True if this process owns a console window.

    A console-hosted CLI run returns True; the engine running inside the
    frozen GUI (omni-exec.exe is built windowed) returns False."""
    try:
        import ctypes
        return bool(ctypes.windll.kernel32.GetConsoleWindow())
    except Exception:  # noqa: BLE001 — never let a probe break startup
        return True     # assume console: the safe direction (change nothing)


def install_no_console_default():
    """Stop every child process from flashing up its own console window.

    A launch runs DOZENS of short-lived console tools (adb polled once a
    second by wait_for_boot, qemu-img, e2fsprogs...). When the parent has NO
    console of its own -- which is exactly the case inside the frozen GUI,
    built windowed -- Windows gives each of those children a BRAND NEW console
    window. The user sees terminal windows strobing across the screen for the
    whole boot.

    Applied as a Popen default rather than at ~69 call sites, so a new
    subprocess call cannot reintroduce the flicker by forgetting a flag.

    Deliberately scoped to the no-console case: when the engine IS running in
    a terminal, children inherit that console, no window is created, and this
    changes nothing. Calls that already set `creationflags` are left alone --
    the QEMU spawns pass DETACHED_PROCESS, which CREATE_NO_WINDOW would
    conflict with (Windows ignores it alongside DETACHED/NEW_CONSOLE anyway).
    """
    if not IS_WINDOWS or _has_own_console():
        return False
    import subprocess
    if getattr(subprocess.Popen, "_omni_no_window", False):
        return False                      # idempotent
    original = subprocess.Popen.__init__

    def _init(self, *args, **kwargs):
        if not kwargs.get("creationflags"):
            kwargs["creationflags"] = CREATE_NO_WINDOW
        return original(self, *args, **kwargs)

    subprocess.Popen.__init__ = _init
    subprocess.Popen._omni_no_window = True
    return True
