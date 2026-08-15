#!/usr/bin/env python3
"""Private DNS: the guest resolves over TLS, so an intercepting network cannot
break the asset CDN.

    python3 tests/test_private_dns.py

WHY THIS EXISTS. A playable instance logged in, joined its place, never
finished loading it, and minutes later showed "Disconnected (Error Code: 277)".
That reads as a performance problem and is not one. logcat had hundreds of

    HttpError: DnsResolve   Could not resolve host: fts.rbxcdn.com

while `google.com`, `roblox.com` and `cloudflare.com` all resolved in the same
guest. Measured against the host's own resolvers and two public ones, the
Roblox asset CDN failed over plaintext UDP:53 EVERYWHERE and succeeded over
DNS-over-HTTPS — i.e. transparent interception on the network path, which no
choice of resolver address can route around. Android's Private DNS
(DNS-over-TLS, TCP:853, certificate pinned to the resolver's hostname) is the
escape, and it fixed all three blocked hosts on a live instance.

These tests pin the parts that are decidable without a guest: the settings
that get written, that it runs on every boot rather than only a playable one,
and that it can be turned off.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _acct():
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": "x86"}


class _Reply:
    def __init__(self, stdout="", stderr=""):
        self.stdout, self.stderr = stdout, stderr


def _run(acct=None, cfg=None, canary="PING t0.rbxcdn.com (1.2.3.4)", env=None):
    """Run ensure_private_dns with adb captured; returns (result, calls)."""
    calls = []

    def fake_adb(a, *args, **kw):
        calls.append(list(args))
        return _Reply(canary if "ping" in " ".join(str(x) for x in args) else "")

    with mock.patch.dict(os.environ, env or {}, clear=False), \
         mock.patch.object(omni, "adb", fake_adb), \
         mock.patch.object(omni.time, "sleep"):
        if env is None:
            os.environ.pop("OMNI_PRIVATE_DNS", None)
        return omni.ensure_private_dns(acct or _acct(), cfg, "t"), calls


def _settings_written(calls):
    """{key: value} for every `settings put global k v` that was issued."""
    out = {}
    for c in calls:
        flat = [str(x) for x in c]
        if flat[:4] == ["shell", "settings", "put", "global"]:
            out[flat[4]] = flat[5]
    return out


class WhatItWrites(unittest.TestCase):
    def test_it_turns_on_dns_over_tls_by_hostname(self):
        _, calls = _run()
        wrote = _settings_written(calls)
        # `hostname` mode, NOT `opportunistic`: opportunistic upgrades the
        # resolver it was already given and accepts any certificate, so an
        # interceptor can still answer. Only hostname mode pins the name.
        self.assertEqual(wrote.get("private_dns_mode"), "hostname")
        self.assertEqual(wrote.get("private_dns_specifier"),
                         omni.DEFAULT_PRIVATE_DNS)

    def test_the_resolver_is_a_hostname_not_an_address(self):
        # The certificate is pinned to this name; an IP literal cannot be.
        self.assertNotRegex(omni.DEFAULT_PRIVATE_DNS, r"^\d+\.\d+\.\d+\.\d+$")

    def test_it_verifies_with_a_host_that_only_fails_when_dns_is_tampered(self):
        _, calls = _run()
        pinged = " ".join(" ".join(str(x) for x in c) for c in calls)
        self.assertIn(omni._DNS_CANARY, pinged)
        self.assertIn("rbxcdn", omni._DNS_CANARY)


class WhenItReportsSuccess(unittest.TestCase):
    def test_a_resolving_canary_is_success(self):
        ok, _ = _run(canary="PING t0.rbxcdn.com (65.9.9.110) 56(84) bytes")
        self.assertTrue(ok)

    def test_an_unresolvable_canary_is_failure(self):
        ok, _ = _run(canary="ping: unknown host t0.rbxcdn.com")
        self.assertFalse(ok)

    def test_it_never_raises_when_adb_is_gone(self):
        """Best-effort, and SystemExit too: adb's _require_adb_port calls
        fail(), which sys.exits, and a boot-tail tune-up must never be able to
        end the process."""
        for boom in (RuntimeError("adb died"), SystemExit(2)):
            with mock.patch.object(omni, "adb", side_effect=boom), \
                 mock.patch.object(omni.time, "sleep"):
                self.assertFalse(omni.ensure_private_dns(_acct(), None, "t"))

    def test_no_adb_endpoint_is_a_no_op(self):
        ok, calls = _run(acct={"name": "u1"})
        self.assertFalse(ok)
        self.assertEqual(calls, [])


class TurningItOff(unittest.TestCase):
    def test_env_off_writes_nothing(self):
        ok, calls = _run(env={"OMNI_PRIVATE_DNS": "off"})
        self.assertFalse(ok)
        self.assertEqual(calls, [])

    def test_config_chooses_the_resolver(self):
        _, calls = _run(cfg={"network": {"private_dns": "one.one.one.one"}})
        self.assertEqual(_settings_written(calls)["private_dns_specifier"],
                         "one.one.one.one")

    def test_env_beats_config(self):
        _, calls = _run(cfg={"network": {"private_dns": "one.one.one.one"}},
                        env={"OMNI_PRIVATE_DNS": "dns.quad9.net"})
        self.assertEqual(_settings_written(calls)["private_dns_specifier"],
                         "dns.quad9.net")

    def test_config_off_is_honoured_too(self):
        self.assertIsNone(
            omni.private_dns_hostname({"network": {"private_dns": "off"}}))


class ItRunsOnEveryBoot(unittest.TestCase):
    """Not a playable-mode tune-up. A farming instance that cannot fetch
    assets is a farming instance that never gets into the game, so this sits
    in the shared boot tail above the profile branch."""

    def test_it_is_called_from_the_shared_boot_tail(self):
        import inspect
        src = inspect.getsource(omni._ensure_booted)
        self.assertIn("ensure_private_dns(", src)
        # Above the density/performance split, i.e. every mode reaches it.
        self.assertLess(src.index("ensure_private_dns("),
                        src.index('if profile == "density"'))

    def test_it_runs_before_the_kiosk_launches_the_game(self):
        """A resolver swapped in underneath a running client leaves the
        failed lookups already cached as failures."""
        import inspect
        src = inspect.getsource(omni._ensure_booted)
        self.assertLess(src.index("ensure_private_dns("),
                        src.index("assert_kiosk_game("))


if __name__ == "__main__":
    unittest.main()
