"""OmniDroid — multi-account Android (Roblox) instance manager."""

__version__ = "0.2.0"

# Windows + no console of our own (i.e. embedded in the frozen GUI) means
# every short-lived child -- adb, qemu-img, e2fsprogs -- would otherwise pop
# its own console window, strobing across the screen for a whole boot. Must
# run before anything spawns a subprocess, so it lives at package import.
from omnidroid.config import install_no_console_default  # noqa: E402

install_no_console_default()
