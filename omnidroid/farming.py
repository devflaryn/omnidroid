"""Farming-mode runtime squeeze.

A joined-but-idle Roblox instance is pushed as small as stable AFTER boot, over
adb, without a separate image. This module only BUILDS the command sequence
(pure, unit-testable); the engine applies it on a farming-mode boot.

Levers (each a step below):
  - stop residual services the trim left running but farming doesn't need;
  - throttle the backgrounded game process via a CPU cgroup/cpuset cap so a
    joined-idle instance does minimal work (safe: instances are headless, no
    active render surface);
  - enable zram swap so the guest can reclaim under the low mem cap;
  - tune lmkd (lowmemorykiller) thresholds to reclaim hard WITHOUT OOM-killing
    the game itself.

The exact service list / thresholds are refined by live measurement (the B1
runbooks); the shape here is the contract."""

# The Roblox package the farming instance keeps joined-idle.
GAME_PKG = "com.roblox.client"


def build_squeeze_sequence():
    """Ordered list of adb `shell` argv vectors for the farming squeeze."""
    return [
        # 1) Quiesce residual background work farming doesn't need. Safe on a
        #    headless kiosk instance; the live runbook extends this list with
        #    specific services proven idle-safe by measurement.
        ["shell", "cmd", "activity", "idle-maintenance"],
        ["shell", "settings", "put", "global", "window_animation_scale", "0"],
        # 2) zram swap on, so the guest reclaims under the low mem cap.
        ["shell", "sh", "-c",
         "swapon /dev/block/zram0 2>/dev/null || "
         "(zramctl -f -s 256M 2>/dev/null; mkswap /dev/block/zram0 2>/dev/null; "
         "swapon /dev/block/zram0 2>/dev/null); true"],
        # 3) lmkd: reclaim aggressively but keep the game alive.
        ["shell", "sh", "-c",
         "setprop ro.lmk.use_psi true; setprop ro.lmk.critical_upgrade true; "
         "setprop ctl.restart lmkd; true"],
        # 4) Throttle the backgrounded game via a cpuset (background cores).
        ["shell", "sh", "-c",
         f"PID=$(pidof {GAME_PKG} 2>/dev/null); "
         f"[ -n \"$PID\" ] && echo $PID > /dev/cpuset/background/tasks "
         f"2>/dev/null; true"],
    ]
