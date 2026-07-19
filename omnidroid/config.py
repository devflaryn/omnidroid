# omnidroid/config.py
"""Single source of truth for platform, paths, and QEMU-binary resolution.

Every path/arch decision in OmniDroid routes through here so one checkout runs
on Windows, macOS, and Linux without any other module knowing the branches.
"""
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
CONFIG_PATH = REPO / "configs" / "paths.json"
ACCOUNTS_DIR = REPO / "accounts"
QEMU_DIR = REPO / "qemu"          # local (auto-installed) QEMU lives here
IS_WINDOWS = platform.system() == "Windows"
IS_LINUX = platform.system() == "Linux"
IS_MACOS = platform.system() == "Darwin"
# Host CPU architecture. arm64 Macs (Apple Silicon) run the arm64 base
# natively under HVF with NO translation layer; x86_64 hosts run the Bliss
# x86 base under WHPX/KVM. Base selection keys off this (see host_arch_base).
HOST_ARCH = platform.machine().lower()
IS_ARM64_HOST = HOST_ARCH in ("arm64", "aarch64")


def resolve_images_dir(cfg):
    """images_dir may be a plain string or a per-platform dict
    ({"windows": ..., "linux": ...}) so one checkout works on both hosts.
    ~ is expanded; a relative path is resolved against the project root
    (default: images/ inside the checkout, travels with the repo)."""
    v = cfg["images_dir"]
    if isinstance(v, dict):
        key = "windows" if IS_WINDOWS else "darwin" if IS_MACOS else "linux"
        # Back-compat: older configs only have windows/linux; macOS falls
        # back to the linux path convention.
        v = v.get(key) or (v.get("linux") if IS_MACOS else None) \
            or v.get("default")
        if not v:
            sys.exit(f"error: configs/paths.json images_dir has no entry "
                     f"for platform '{key}'")
    p = Path(v).expanduser()
    if not p.is_absolute():
        p = REPO / p
    return str(p)


# ---------- qemu resolution + auto-install ----------

def qemu_bin(tool):
    """Resolve a QEMU executable path from the PRODUCT directory only.
    Order: config 'qemu.dir' (an explicit product-side override) -> local
    QEMU_DIR (auto-downloaded, next to the engine). On Windows the shipped
    product NEVER falls back to a host/global/PATH QEMU: if it isn't in the
    product dir yet, the returned (non-existent) product path drives
    ensure_qemu() to download it there. On Linux/macOS the documented model is
    SYSTEM QEMU (apt/brew), so a bare name (PATH) is the final fallback."""
    exe = tool + (".exe" if IS_WINDOWS else "")
    try:
        from omnidroid import engine
        qd = engine.read_config().get("qemu", {}).get("dir")
    except Exception:
        qd = None
    for cand in ([Path(qd) / exe] if qd else []) + [QEMU_DIR / exe]:
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
