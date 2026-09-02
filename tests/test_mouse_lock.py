"""Mouse-look confinement: the game says when, the host obeys (mouselock.py).

Pure parts only -- the QMP command is qemu-patches/0013 and the Android
capture is the -lock APK build; both are exercised by booting."""
import unittest

from omnidroid import mouselock
from omnidroid.mouselock import lock_port, lock_url, parse_request, script_for


class Ports(unittest.TestCase):
    def test_port_is_per_instance(self):
        self.assertNotEqual(lock_port({"qmp_port": 17001}),
                            lock_port({"qmp_port": 17002}))

    def test_url_is_the_host_seen_from_slirp(self):
        self.assertEqual(lock_url({"qmp_port": 17001}),
                         "http://10.0.2.2:19001/lock")


class Script(unittest.TestCase):
    def test_script_carries_this_instances_url_and_no_placeholder(self):
        s = script_for({"qmp_port": 17003})
        self.assertEqual(s["name"], mouselock.SCRIPT_NAME)
        self.assertIn("http://10.0.2.2:19003/lock", s["body"])
        self.assertNotIn("__OMNI_LOCK_URL__", s["body"])

    def test_script_keys_on_the_games_own_property_not_a_button(self):
        body = script_for({"qmp_port": 17001})["body"]
        self.assertIn("MouseBehavior", body)
        self.assertNotIn("MouseButton2", body)
        self.assertNotIn("UserInputType", body)

    def test_script_runs_first(self):
        self.assertTrue(mouselock.SCRIPT_NAME.startswith("00_"))


class Requests(unittest.TestCase):
    def test_lock_and_unlock(self):
        self.assertEqual(parse_request("/lock?on=1"), (True, False))
        self.assertEqual(parse_request("/lock?on=0"), (False, False))

    def test_heartbeat_rides_on_the_state_or_alone(self):
        self.assertEqual(parse_request("/lock?on=0&alive=1"), (False, True))
        self.assertEqual(parse_request("/lock?alive=1"), (None, True))

    def test_anything_else_is_ignored(self):
        self.assertEqual(parse_request("/"), (None, False))
        self.assertEqual(parse_request("/lock"), (None, False))
        self.assertEqual(parse_request("/lock?on=maybe"), (None, False))
        self.assertEqual(parse_request("/unlock?on=1"), (None, False))


class Heartbeat(unittest.TestCase):
    """A fresh ping means 'in a place'; a stale one means UNKNOWN, never
    'left' -- the executor may simply be unhealthy, and the pointer policy
    fails towards visible on unknown."""

    def test_fresh_ping_is_in_place(self):
        mouselock.note_alive("hb-a", now=100.0)
        self.assertTrue(mouselock.in_place_by_heartbeat("hb-a", now=103.0))

    def test_stale_ping_is_unknown(self):
        mouselock.note_alive("hb-b", now=100.0)
        self.assertIsNone(mouselock.in_place_by_heartbeat("hb-b", now=120.0))

    def test_never_pinged_is_unknown(self):
        self.assertIsNone(mouselock.in_place_by_heartbeat("hb-never"))

    def test_the_script_pings(self):
        self.assertIn("alive=1", mouselock.LUA_SCRIPT)


class EngineSide(unittest.TestCase):
    def test_support_is_read_from_the_capability_token(self):
        from omnidroid import qemu_proc
        real = qemu_proc._omni_caps_of
        try:
            qemu_proc._omni_caps_of = lambda b: ("omni-window", "omni-pointer-lock")
            self.assertTrue(qemu_proc.qemu_supports_pointer_lock())
            qemu_proc._omni_caps_of = lambda b: ("omni-window",)
            self.assertFalse(qemu_proc.qemu_supports_pointer_lock())
        finally:
            qemu_proc._omni_caps_of = real

    def test_gaming_argv_carries_a_relative_pointer_behind_the_tablet(self):
        from omnidroid import qemu_proc
        argv = qemu_proc.usb_devices({"usb": True}, arm=False, cfg={})
        s = " ".join(argv)
        self.assertIn("virtio-mouse-pci", s)
        self.assertLess(s.index("virtio-mouse-pci"), s.index("virtio-tablet-pci"))


if __name__ == "__main__":
    unittest.main()
