#!/usr/bin/env python3
"""Dev mode moves the GUEST's packets, and only in dev mode.

The executor's server address is compiled into its native library, so the guest
cannot be *told* to use a different one -- omnidroid rewrites the destination
with an iptables DNAT rule instead (see omnidroid/devserver.py). Three things
about that are worth holding still:

  * A loopback dev server must become 10.0.2.2. The host's 127.0.0.1 is the
    GUEST's own loopback from inside the VM, so a rule aimed at 127.0.0.1 would
    silently redirect every call into a closed port on the guest -- a dev mode
    that looks applied and drops the traffic.
  * The rule must be idempotent. It is installed on every boot, and warm-pool
    slots boot repeatedly; a stacking `-A` would leave a chain that takes
    longer to walk on each pass.
  * OFF must cost nothing. The production path does not reach adb at all, and a
    boot that cannot install the rule must still be a boot.

    python3 tests/test_dev_server_redirect.py     (or: pytest tests/)
"""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import devserver as d  # noqa: E402
from omnidroid import engine as omni  # noqa: E402


class Target(unittest.TestCase):
    """dev_target(): the switch, and what the guest is told to dial."""

    def test_off_by_default(self):
        self.assertIsNone(d.dev_target(env={}, config_path="/nonexistent"))

    def test_bare_flag_is_the_hosts_backend(self):
        self.assertEqual(d.dev_target(env={d.DEV_ENV: "1"},
                                      config_path="/nonexistent"),
                         "10.0.2.2:5500")

    def test_loopback_becomes_the_slirp_alias(self):
        """The whole reason this module does not just pass the string through:
        127.0.0.1 inside the guest is the guest."""
        for value in ("127.0.0.1:5500", "http://127.0.0.1:5500",
                      "localhost:5500", "http://localhost:5500/"):
            with self.subTest(value=value):
                self.assertEqual(
                    d.dev_target(env={d.DEV_ENV: value},
                                 config_path="/nonexistent"),
                    "10.0.2.2:5500")

    def test_a_lan_address_is_left_alone(self):
        """A backend on another box is reachable from the guest as itself."""
        self.assertEqual(d.dev_target(env={d.DEV_ENV: "http://10.0.0.4:8080"},
                                      config_path="/nonexistent"),
                         "10.0.0.4:8080")

    def test_a_host_with_no_port_gets_the_backend_default(self):
        self.assertEqual(d.dev_target(env={d.DEV_ENV: "10.0.0.4"},
                                      config_path="/nonexistent"),
                         f"10.0.0.4:{d.DEFAULT_DEV_PORT}")

    def test_falsey_env_is_off(self):
        for value in ("0", "false", "off", "no", "  "):
            with self.subTest(value=value):
                self.assertIsNone(d.dev_target(env={d.DEV_ENV: value},
                                               config_path="/nonexistent"))

    def test_dev_json_works_without_an_env_var(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "dev.json"
            p.write_text(json.dumps({"devServer": "127.0.0.1:5500"}))
            self.assertEqual(d.dev_target(env={}, config_path=p),
                             "10.0.2.2:5500")

    def test_dev_mode_false_in_the_file_is_off(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "dev.json"
            p.write_text(json.dumps({"devMode": False,
                                     "devServer": "127.0.0.1:5500"}))
            self.assertIsNone(d.dev_target(env={}, config_path=p))

    def test_unreadable_dev_json_is_off_not_a_crash(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "dev.json"
            p.write_text("{not json")
            self.assertIsNone(d.dev_target(env={}, config_path=p))

    def test_env_beats_the_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "dev.json"
            p.write_text(json.dumps({"devServer": "10.0.0.4:9999"}))
            self.assertEqual(
                d.dev_target(env={d.DEV_ENV: "10.0.0.5:1234"}, config_path=p),
                "10.0.0.5:1234")


class Rule(unittest.TestCase):
    """dnat_script(): what actually runs in the guest."""

    def test_rewrites_only_the_production_address(self):
        script = d.dnat_script("10.0.2.2:5500")
        self.assertIn(f"-d {d.PROD_IP}", script)
        self.assertIn("--to-destination 10.0.2.2:5500", script)

    def test_uses_the_output_chain(self):
        """The traffic ORIGINATES in the guest; PREROUTING would never see it."""
        self.assertIn("-t nat", d.dnat_script("10.0.2.2:5500"))
        self.assertIn("OUTPUT", d.dnat_script("10.0.2.2:5500"))
        self.assertNotIn("PREROUTING", d.dnat_script("10.0.2.2:5500"))

    def test_is_idempotent(self):
        """`-C` first: this runs on every boot, and warm-pool slots boot often."""
        script = d.dnat_script("10.0.2.2:5500")
        self.assertIn("-C", script)
        self.assertLess(script.index("-C"), script.index("-A"),
                        "the existence check must run before the append")


class Applied(unittest.TestCase):
    """engine.apply_dev_redirect(): the never-fail-a-boot contract."""

    def setUp(self):
        self.acct = {"name": "t"}

    def test_off_does_not_touch_adb(self):
        with mock.patch.object(d, "dev_target", return_value=None), \
                mock.patch.object(omni, "adb") as adb:
            self.assertFalse(omni.apply_dev_redirect(self.acct, "t"))
        adb.assert_not_called()

    def test_on_installs_the_rule_and_reports_it(self):
        r = mock.Mock(stdout="1\n", stderr="")
        with mock.patch.object(d, "dev_target", return_value="10.0.2.2:5500"), \
                mock.patch.object(omni, "adb", return_value=r) as adb:
            self.assertTrue(omni.apply_dev_redirect(self.acct, "t"))
        sent = adb.call_args[0][2]
        self.assertIn(d.PROD_IP, sent)
        self.assertIn("10.0.2.2:5500", sent)

    def test_a_guest_without_iptables_does_not_fail_the_boot(self):
        with mock.patch.object(d, "dev_target", return_value="10.0.2.2:5500"), \
                mock.patch.object(omni, "adb",
                                  side_effect=RuntimeError("no iptables")):
            self.assertFalse(omni.apply_dev_redirect(self.acct, "t"))

    def test_a_rule_that_did_not_take_is_reported_as_a_failure(self):
        """iptables exiting 0 is not proof: the count is read back, exactly like
        block_external_hosts reads state back rather than trusting the send. A
        dev session that silently kept talking to production is the one outcome
        worth failing loudly for."""
        r = mock.Mock(stdout="0\n", stderr="")
        with mock.patch.object(d, "dev_target", return_value="10.0.2.2:5500"), \
                mock.patch.object(omni, "adb", return_value=r):
            self.assertFalse(omni.apply_dev_redirect(self.acct, "t"))


class NotShipped(unittest.TestCase):
    """The module is excluded from the frozen build, so engine.py must work
    without it — that import guard is what a customer runs."""

    def test_engine_imports_it_defensively(self):
        src = (Path(omni.__file__).read_text(encoding="utf-8"))
        self.assertIn("from . import devserver as _devserver", src)
        self.assertIn("except ImportError", src)

    def test_absent_module_is_a_no_op(self):
        with mock.patch.object(omni, "_devserver", None), \
                mock.patch.object(omni, "adb") as adb:
            self.assertFalse(omni.apply_dev_redirect({"name": "t"}, "t"))
        adb.assert_not_called()


if __name__ == "__main__":
    unittest.main(verbosity=2)
