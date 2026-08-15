"""What MTU the guest may use, given how this host actually reaches the world.

The failure this exists to stop
------------------------------
QEMU's user networking hands the guest a 1500-byte MTU and then sends the
guest's packets out through the HOST's stack. When the host's egress is a
tunnel — a VPN, PPPoE, anything encapsulated — its real MTU is smaller, and
the difference is invisible to the guest.

TCP survives that: the two ends negotiate an MSS and the host's stack clamps
it. **UDP does not negotiate anything**, and Roblox's gameplay traffic is UDP.
So the split signature is:

    assets load fine (HTTPS/TCP)  →  the game's own loading screen appears
    the game server connects      →  "Connection accepted from 128.116.x.x"
    then it dies under load       →  "Disconnected (Error Code: 277)"

MEASURED 2026-08-15 on this host: ProtonVPN's adapter reports **MTU 1420**,
the guest's `eth0` reported **1500**, and PS99 disconnected every time once
the world started streaming. That looks exactly like a broken emulator and is
not one.

`virtio-net-pci,host_mtu=N` is the fix: virtio has a feature bit for exactly
this (`VIRTIO_NET_F_MTU`), so the guest kernel brings `eth0` up at N without
anything running inside the guest. Android 13's kernel supports it.

Why detect rather than hardcode
-------------------------------
Capping every guest at 1400 would work everywhere and quietly cost throughput
on the hosts that do not need it. Reading the host's own egress MTU is the
same one-line answer with none of that, and it self-corrects when the user
connects or drops a VPN between launches.
"""

import socket
import sys

# The standard Ethernet MTU. A guest at this size is what QEMU does today, and
# what every non-tunnelled host wants.
DEFAULT_MTU = 1500
# Below this something is badly wrong (or someone typo'd a config); IPv6
# requires 1280 as an absolute floor, so refuse anything under it rather than
# hand QEMU a value that makes the guest unable to reach anything at all.
MIN_MTU = 1280


def sanitize(value, default=DEFAULT_MTU):
    """A usable MTU from whatever a config/env/probe produced, or `default`."""
    try:
        n = int(value)
    except (TypeError, ValueError):
        return default
    if n < MIN_MTU or n > 9000:
        return default
    return n


def _local_ip_for_internet():
    """The source address this host would use to reach the internet.

    A connected UDP socket performs no I/O — it only asks the routing table —
    so this is fast and works with no network traffic and no name lookup."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 53))
        return s.getsockname()[0]
    except OSError:
        return None
    finally:
        s.close()


def _windows_mtu_for(local_ip, timeout=8):
    """MTU of the Windows IP interface that owns `local_ip`, or None.

    Deliberately the IP-layer **NlMtu**, not the adapter's link MTU. A VPN's
    TUN driver (Wintun, and the OpenVPN tap) reports a link MTU of 65535 while
    the IP interface is configured at the tunnel's real size -- MEASURED here
    on ProtonVPN: `GetAdaptersAddresses` said 65535, `Get-NetIPInterface` said
    **1420**, and 1420 is the number the packets actually have to fit in. A
    probe that read the first one would confidently report "no tunnel" on
    exactly the hosts that have one.

    One PowerShell call rather than ctypes over MIB_IPINTERFACE_ROW: that
    struct is ~180 bytes of fields whose offsets have to be exactly right, and
    getting it subtly wrong reads as a plausible number rather than as an
    error -- which is the failure mode this function exists to avoid. It runs
    once per boot, against a 50-190 s boot.
    """
    import subprocess
    cmd = ["powershell", "-NoProfile", "-NonInteractive", "-Command",
           f"(Get-NetIPAddress -IPAddress '{local_ip}' -AddressFamily IPv4 "
           f"-ErrorAction Stop | Get-NetIPInterface -ErrorAction Stop)"
           f".NlMtu"]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True,
                           timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return None
    for line in (r.stdout or "").splitlines():
        line = line.strip()
        if line.isdigit():
            return int(line)
    return None


def _linux_mtu_for(local_ip):
    """MTU of the Linux interface holding `local_ip`, via /sys."""
    import os
    base = "/sys/class/net"
    try:
        names = os.listdir(base)
    except OSError:
        return None
    for name in names:
        try:
            with open(f"{base}/{name}/mtu") as fh:
                mtu = int(fh.read().strip())
        except (OSError, ValueError):
            continue
        # Match the interface by asking the kernel for its address.
        try:
            import fcntl
            import struct
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                addr = socket.inet_ntoa(fcntl.ioctl(
                    s.fileno(), 0x8915,      # SIOCGIFADDR
                    struct.pack("256s", name[:15].encode()))[20:24])
            finally:
                s.close()
        except OSError:
            continue
        if addr == local_ip:
            return mtu
    return None


# Probed once per PROCESS, not once per call: a boot asks for it from the argv
# builder and again for the log line, and the Windows probe spawns PowerShell.
# Per-process rather than persisted, because a VPN can come up between two
# launches and the next `start` must see the new number.
_CACHE = {}


def host_egress_mtu():
    """The MTU of the interface this host would reach the internet through,
    or None when it cannot be determined.

    Never raises: a probe that cannot answer must leave the caller with the
    ordinary 1500-byte default, not fail a boot."""
    if "mtu" in _CACHE:
        return _CACHE["mtu"]
    _CACHE["mtu"] = _probe_host_egress_mtu()
    return _CACHE["mtu"]


def _probe_host_egress_mtu():
    try:
        ip = _local_ip_for_internet()
        if not ip:
            return None
        if sys.platform == "win32":
            return _windows_mtu_for(ip)
        if sys.platform.startswith("linux"):
            return _linux_mtu_for(ip)
        return None            # macOS: set network.mtu if you tunnel there
    except Exception:          # noqa: BLE001 - a probe, on the boot path
        return None


def guest_mtu(cfg=None, env=None, probe=None):
    """(mtu, why) for this boot: env -> config -> host probe -> 1500.

    `why` is for the launch log, because a silently-lowered MTU is exactly the
    kind of invisible decision that costs an afternoon later.

    `probe` is resolved HERE rather than as a default argument: a default
    binds the function object at definition time, so a caller (or a test) that
    replaces `netmtu.host_egress_mtu` would be ignored -- silently, and only
    in the direction that matters."""
    probe = probe or host_egress_mtu
    env = env if env is not None else __import__("os").environ
    raw = env.get("OMNI_GUEST_MTU")
    if raw:
        return sanitize(raw), "OMNI_GUEST_MTU"
    raw = ((cfg or {}).get("network") or {}).get("mtu")
    if raw:
        return sanitize(raw), "config network.mtu"
    found = probe()
    if found and found < DEFAULT_MTU:
        return sanitize(found), "this host's egress interface"
    return DEFAULT_MTU, "default"
