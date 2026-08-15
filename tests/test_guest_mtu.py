"""The guest's MTU has to fit the HOST's way out.

    python3 -m pytest tests/test_guest_mtu.py -q

QEMU's user networking hands the guest 1500 and then sends its packets out
through the host's stack. Behind a VPN that stack is smaller, TCP survives it
(MSS negotiation) and **UDP does not** -- and Roblox's gameplay traffic is
UDP. MEASURED 2026-08-15: ProtonVPN's IP interface 1420, guest 1500, and PS99
connected to a real game server and then dropped with Error Code 277 every
time the world started streaming.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import netmtu  # noqa: E402
from omnidroid import qemu_proc as qp  # noqa: E402


def test_a_normal_host_changes_nothing():
    mtu, why = netmtu.guest_mtu({}, {}, probe=lambda: 1500)
    assert mtu == 1500 and why == "default"
    assert qp.nic_mtu_suffix.__doc__          # documented, not incidental


def test_a_tunnelled_host_is_detected():
    mtu, why = netmtu.guest_mtu({}, {}, probe=lambda: 1420)
    assert mtu == 1420
    assert "egress" in why


def test_a_probe_that_cannot_answer_leaves_the_default():
    # A boot must never fail because a capability probe did.
    assert netmtu.guest_mtu({}, {}, probe=lambda: None) == (1500, "default")


def test_the_probe_never_raises_into_a_boot():
    def explode():
        raise OSError("no network stack today")
    try:
        netmtu.guest_mtu({}, {}, probe=explode)
    except OSError:
        raised = True
    else:
        raised = False
    # guest_mtu itself does not swallow (the PROBE does), so document which
    # one is the guard: host_egress_mtu is what callers actually pass.
    assert raised, "guest_mtu passes the probe's failure through by design"
    netmtu._CACHE.clear()
    assert netmtu.host_egress_mtu() is not None or True   # never raises


def test_env_and_config_beat_the_probe():
    assert netmtu.guest_mtu({"network": {"mtu": 1350}}, {},
                            probe=lambda: 1420)[0] == 1350
    assert netmtu.guest_mtu({"network": {"mtu": 1350}},
                            {"OMNI_GUEST_MTU": "1300"},
                            probe=lambda: 1420)[0] == 1300


def test_absurd_values_are_refused_not_passed_to_qemu():
    # A guest below the IPv6 minimum cannot reach anything; a typo must not
    # be able to produce one.
    assert netmtu.sanitize(68) == 1500
    assert netmtu.sanitize(100000) == 1500
    assert netmtu.sanitize("not a number") == 1500
    assert netmtu.sanitize(1280) == 1280


def test_the_nic_only_carries_host_mtu_when_it_is_smaller(monkeypatch):
    monkeypatch.setattr(netmtu, "host_egress_mtu", lambda: 1420)
    monkeypatch.delenv("OMNI_GUEST_MTU", raising=False)
    assert qp.nic_mtu_suffix({}) == ",host_mtu=1420"
    monkeypatch.setattr(netmtu, "host_egress_mtu", lambda: 1500)
    # An unconditional host_mtu=1500 would be a no-op that still changes every
    # command line (and every warm-cache/pool key derived from one).
    assert qp.nic_mtu_suffix({}) == ""
