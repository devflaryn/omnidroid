#!/usr/bin/env python3
"""Re-registering the dev base must PRESERVE an existing base_disk and notes.

`build_dev_base` rewrites bases.dev wholesale. Copying base_disk from the arm
base silently repoints dev from base_arm.qcow2 to base_arm_v2.qcow2 -- changing
which image the dev guest actually boots, with no log line and no opt-in. The
dev system image is STANDALONE (it shadows the shared base -- see
_brand_target), so its base_disk is an independent choice, not something to
inherit from the arm base on every rebuild.

    python3 tests/test_dev_base_registration.py     (or: pytest tests/)
"""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


class DevBaseReRegistration(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-devreg-"))
        self.cfg_path = self.tmp / "paths.json"
        self.existing = {
            "current_base": "x86",
            "bases": {
                "arm": {"type": "arm-uefi", "base_disk": "base_arm_v2.qcow2",
                        "system": "base_arm_system.qcow2",
                        "data": "base_arm_data.qcow2",
                        "efivars": "base_arm_efivars.fd"},
                "dev": {"type": "arm-uefi", "base_disk": "base_arm.qcow2",
                        "system": "base_arm_devsystem.qcow2",
                        "data": "base_arm_devdata.qcow2",
                        "notes": "hand-written note that must survive"},
            },
        }
        self.cfg_path.write_text(json.dumps(self.existing))

    def _entry(self, raw=None, rooted=True):
        raw = raw if raw is not None else json.loads(self.cfg_path.read_text())
        arm = raw["bases"]["arm"]
        return omni._dev_base_entry(
            raw, arm, devkit_disk="base_arm_devkit.qcow2",
            dev_data="base_arm_devdata.qcow2", frida_version="17.15.4",
            frida_port=27142, magisk=True, magisk_version="v30.7",
            rooted=rooted)

    def test_existing_base_disk_is_preserved(self):
        entry = self._entry()
        self.assertEqual(entry["base_disk"], "base_arm.qcow2",
                         "re-registration must not repoint dev at the arm base disk")

    def test_existing_notes_are_preserved(self):
        entry = self._entry()
        self.assertIn("hand-written note", entry["notes"])

    def test_rooted_flag_still_updates(self):
        entry = self._entry(rooted=True)
        self.assertTrue(entry["devkit_manifest"]["rooted"])
        entry = self._entry(rooted=False)
        self.assertFalse(entry["devkit_manifest"]["rooted"])

    def test_system_always_tracks_the_canonical_devsystem(self):
        # `system` is NOT sticky: build-dev-base owns which devsystem file the
        # dev base uses, so a temporary repoint (e.g. a probe image) is not
        # silently made permanent by a rebuild.
        raw = json.loads(self.cfg_path.read_text())
        raw["bases"]["dev"]["system"] = "base_arm_devsystem_probe.qcow2"
        entry = self._entry(raw)
        self.assertEqual(entry["system"], omni.ARM_DEVSYSTEM_DISK)

    def test_preserved_notes_get_a_truthful_root_marker(self):
        # Sticky notes must not outlive the fact they assert. A preserved note
        # saying "root pending" after the boot HAS been patched would contradict
        # devkit_manifest.rooted sitting directly below it.
        raw = json.loads(self.cfg_path.read_text())
        raw["bases"]["dev"]["notes"] = (
            "arm dev base: hand-written context [root pending: --patch-boot]. "
            "hidden frida port 27142.")
        entry = self._entry(raw, rooted=True)
        self.assertIn("hand-written context", entry["notes"],
                      "the human-written part of the note must survive")
        self.assertNotIn("root pending", entry["notes"])
        self.assertIn("[rooted]", entry["notes"])

    def test_root_marker_downgrades_when_root_is_lost(self):
        raw = json.loads(self.cfg_path.read_text())
        raw["bases"]["dev"]["notes"] = "arm dev base: context [rooted]. port 27142."
        entry = self._entry(raw, rooted=False)
        self.assertIn("context", entry["notes"])
        self.assertNotIn("[rooted]", entry["notes"])
        self.assertIn("root pending", entry["notes"])

    def test_notes_without_a_marker_are_left_alone(self):
        raw = json.loads(self.cfg_path.read_text())
        entry = self._entry(raw, rooted=True)
        self.assertEqual(entry["notes"], "hand-written note that must survive")

    def test_fresh_registration_falls_back_to_arm_base_disk(self):
        raw = json.loads(self.cfg_path.read_text())
        del raw["bases"]["dev"]
        entry = self._entry(raw)
        self.assertEqual(entry["base_disk"], "base_arm_v2.qcow2",
                         "with no existing dev entry, inherit the arm base disk")

    def test_fresh_registration_generates_notes_reflecting_root_state(self):
        raw = json.loads(self.cfg_path.read_text())
        del raw["bases"]["dev"]
        self.assertIn("[rooted]", self._entry(raw, rooted=True)["notes"])
        self.assertIn("root pending", self._entry(raw, rooted=False)["notes"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
