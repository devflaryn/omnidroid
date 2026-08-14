#!/usr/bin/env python3
"""Roblox version OFFSETS: many baked versions, one clean base.

    python3 tests/test_offsets.py

The requirement these tests pin, in the order it was stated:

  1. the base ships NO Roblox — baking a version must never modify the base's
     own /data, only add a sibling overlay;
  2. baking a NEW version must not delete or disturb the one already baked;
  3. exactly one version is the DEFAULT, and a bare launch uses it;
  4. offsets are per-LAUNCH, never per-account — nothing about an account
     selects a version, and cookie injection is untouched;
  5. naming a version that is not baked is a HARD error, never a silent
     fallback to the default. Running the wrong Roblox under the right name is
     the single most expensive way for this to be wrong.
"""
import os
import sys
import unittest
from pathlib import PurePosixPath
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bases  # noqa: E402
from omnidroid import offsets  # noqa: E402
from omnidroid import engine as omni  # noqa: E402


PRISTINE = "base_arm_data_rooted.qcow2"


def _base(**kw):
    b = {"type": "arm-uefi", "system": "base_arm_system_rooted.qcow2",
         "data": PRISTINE, "rooted": True,
         "root_manifest": {"rooted_data": PRISTINE}}
    b.update(kw)
    return b


class Naming(unittest.TestCase):
    def test_a_version_string_is_a_valid_name(self):
        self.assertTrue(offsets.valid_offset_name("2.731.944"))

    def test_letters_dashes_underscores_are_fine(self):
        for n in ("arceus-test", "roblox_v2", "A1"):
            self.assertTrue(offsets.valid_offset_name(n), n)

    def test_the_reserved_none_is_refused(self):
        # `--offset none` MEANS "no offset"; a registry key of that name would
        # make the two indistinguishable.
        self.assertFalse(offsets.valid_offset_name("none"))

    def test_names_that_would_be_unsafe_filenames_are_refused(self):
        for n in ("", "-x", ".hidden", "a b", "a/b", "a" * 49):
            self.assertFalse(offsets.valid_offset_name(n), n)

    def test_the_image_name_sits_beside_the_data_it_overlays(self):
        # An offset lives in the SAME arch subfolder as the pristine /data it
        # overlays. That co-location is what keeps `rebase -u -b <bare name>`
        # (and therefore a relocatable images_dir) working: a qcow2's backing
        # reference resolves relative to the overlay's OWN directory.
        self.assertEqual(offsets.offset_image_name("2.731.944"),
                         "arm/base_arm_data_offset_2.731.944.qcow2")
        self.assertEqual(
            PurePosixPath(offsets.offset_image_name("x")).parent,
            PurePosixPath(bases.ARM_ROOTED_DATA).parent)


class TheBaseStaysClean(unittest.TestCase):
    def test_registering_a_version_never_touches_the_bases_own_data(self):
        b = _base()
        offsets.register_offset(b, "2.731.944", {"data": "o1.qcow2"})
        self.assertEqual(b["data"], PRISTINE)

    def test_every_offset_overlays_the_pristine_data_not_each_other(self):
        # The anti-chaining rule: offsets are SIBLINGS. Chaining would make
        # offset N carry every superseded APK, and deleting one would corrupt
        # the others.
        from omnidroid.bases import data_bake_source
        b = _base()
        offsets.register_offset(b, "a", {"data": "oa.qcow2"})
        offsets.register_offset(b, "bb", {"data": "ob.qcow2"})
        self.assertEqual(data_bake_source(b), PRISTINE)


class AddingAVersionKeepsTheOthers(unittest.TestCase):
    def setUp(self):
        self.b = _base()
        offsets.register_offset(self.b, "old", {"data": "old.qcow2"})
        offsets.register_offset(self.b, "new", {"data": "new.qcow2"})

    def test_both_versions_coexist(self):
        self.assertEqual(sorted(offsets.offsets_of(self.b)), ["new", "old"])

    def test_the_first_one_baked_stays_the_default(self):
        # Baking a test build must not silently repoint production at it.
        self.assertEqual(offsets.default_offset_name(self.b), "old")

    def test_a_new_version_can_ask_to_become_the_default(self):
        offsets.register_offset(self.b, "newer", {"data": "n2.qcow2"},
                                make_default=True)
        self.assertEqual(offsets.default_offset_name(self.b), "newer")


class TheDefault(unittest.TestCase):
    def test_the_only_offset_is_the_default_without_being_told(self):
        b = _base(offsets={"solo": {"data": "s.qcow2"}})
        self.assertEqual(offsets.default_offset_name(b), "solo")

    def test_several_offsets_and_no_recorded_default_is_ambiguous(self):
        # Picking one at random is how you spend an hour debugging the wrong
        # Roblox. It must refuse and say so.
        b = _base(offsets={"a": {"data": "a.qcow2"}, "b": {"data": "b.qcow2"}})
        self.assertIsNone(offsets.default_offset_name(b))
        self.assertEqual(offsets.resolve_offset(b)[2], "ambiguous")

    def test_removing_the_default_repoints_it_at_the_survivor(self):
        b = _base(offsets={"a": {"data": "a.qcow2"}, "b": {"data": "b.qcow2"}},
                  default_offset="a")
        offsets.unregister_offset(b, "a")
        self.assertEqual(b.get("default_offset"), "b")

    def test_removing_the_last_offset_clears_the_default(self):
        # Never left dangling at a name that no longer resolves — that would
        # fail every subsequent bare launch with a confusing error.
        b = _base(offsets={"a": {"data": "a.qcow2"}}, default_offset="a")
        offsets.unregister_offset(b, "a")
        self.assertNotIn("default_offset", b)
        self.assertEqual(offsets.offsets_of(b), {})


class Resolution(unittest.TestCase):
    def setUp(self):
        self.b = _base(offsets={"v1": {"data": "v1.qcow2"},
                                "v2": {"data": "v2.qcow2"}},
                       default_offset="v1")

    def test_no_request_means_the_default(self):
        name, _entry, why = offsets.resolve_offset(self.b)
        self.assertEqual((name, why), ("v1", "default"))

    def test_an_explicit_request_wins(self):
        name, _entry, why = offsets.resolve_offset(self.b, "v2")
        self.assertEqual((name, why), ("v2", "explicit"))

    def test_an_unknown_version_is_never_silently_the_default(self):
        name, _entry, why = offsets.resolve_offset(self.b, "v9")
        self.assertIsNone(name)
        self.assertEqual(why, "unknown")

    def test_none_means_the_clean_base(self):
        name, _entry, why = offsets.resolve_offset(self.b, offsets.NO_OFFSET)
        self.assertIsNone(name)
        self.assertEqual(why, "explicit")

    def test_a_clean_base_with_nothing_baked_reports_none(self):
        self.assertEqual(offsets.resolve_offset(_base())[2], "none")


class MigratingAPreOffsetsInstall(unittest.TestCase):
    """An existing install has `data` pointing at the old single bake. It must
    converge on the offsets model WITHOUT losing the Roblox it runs today."""

    def setUp(self):
        self.b = _base(data=offsets.LEGACY_GAME_DATA,
                       game_baked={"package": "com.roblox.client",
                                   "apk": "roblox.apk"})
        self.moved = offsets.migrate_legacy_bake(self.b)

    def test_the_base_is_clean_afterwards(self):
        self.assertEqual(self.b["data"], PRISTINE)

    def test_the_baked_roblox_is_adopted_rather_than_orphaned(self):
        self.assertEqual(self.moved, "legacy")
        self.assertEqual(offsets.offsets_of(self.b)["legacy"]["data"],
                         offsets.LEGACY_GAME_DATA)

    def test_it_becomes_the_default_so_launches_keep_working(self):
        self.assertEqual(offsets.default_offset_name(self.b), "legacy")

    def test_the_old_game_baked_marker_is_gone(self):
        self.assertNotIn("game_baked", self.b)

    def test_it_is_idempotent(self):
        self.assertIsNone(offsets.migrate_legacy_bake(self.b))
        self.assertEqual(len(offsets.offsets_of(self.b)), 1)

    def test_an_already_clean_base_is_untouched(self):
        b = _base()
        self.assertIsNone(offsets.migrate_legacy_bake(b))
        self.assertEqual(b, _base())


class LaunchResolution(unittest.TestCase):
    """engine.resolve_launch_offset — the launch-time half."""

    def _cfg(self, images, **base_kw):
        return {"images_dir": str(images), "bases": {"arm": _base(**base_kw)}}

    def setUp(self):
        import tempfile
        self.tmp = tempfile.mkdtemp()
        from pathlib import Path
        Path(self.tmp, "v1.qcow2").write_bytes(b"x")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_the_default_resolves_to_its_image(self):
        cfg = self._cfg(self.tmp, offsets={"v1": {"data": "v1.qcow2"}},
                        default_offset="v1")
        self.assertEqual(omni.resolve_launch_offset(cfg, "arm"),
                         ("v1", "v1.qcow2"))

    def test_an_unknown_version_exits(self):
        cfg = self._cfg(self.tmp, offsets={"v1": {"data": "v1.qcow2"}},
                        default_offset="v1")
        with self.assertRaises(SystemExit):
            omni.resolve_launch_offset(cfg, "arm", "v9")

    def test_a_registered_offset_whose_image_vanished_exits(self):
        # A registry entry is not proof of a file. Booting one would fail
        # deep inside QEMU with an unrelated-looking error.
        cfg = self._cfg(self.tmp, offsets={"gone": {"data": "gone.qcow2"}},
                        default_offset="gone")
        with self.assertRaises(SystemExit):
            omni.resolve_launch_offset(cfg, "arm")

    def test_nothing_baked_exits_unless_the_caller_allows_it(self):
        cfg = self._cfg(self.tmp)
        with self.assertRaises(SystemExit):
            omni.resolve_launch_offset(cfg, "arm")
        # --apk supplies the build itself, so a clean boot is correct there.
        self.assertEqual(omni.resolve_launch_offset(cfg, "arm",
                                                    allow_none=True),
                         (None, None))


class OffsetsAreNotPerAccount(unittest.TestCase):
    def test_the_registry_lives_on_the_BASE_not_on_an_account(self):
        b = _base(offsets={"v1": {"data": "v1.qcow2"}})
        self.assertIn("offsets", b)
        # Nothing in the account store/handle shape names an offset as
        # identity: build_acct puts it on the per-BOOT handle only.
        import inspect
        src = inspect.getsource(omni.build_acct)
        self.assertIn("offset", src)
        self.assertNotIn("save_account", src)


class TheQemuCommandOpensTheOffset(unittest.TestCase):
    def test_the_resolved_offset_image_is_what_gets_attached_as_vdb(self):
        from omnidroid import qemu_proc
        from pathlib import Path
        import tempfile
        tmp = tempfile.mkdtemp()
        Path(tmp, "base_arm_efivars.fd").write_bytes(b"E")
        cfg = {"images_dir": tmp,
               "qemu": {"smp": 4, "mem_mb": 4096},
               "bases": {"arm": _base()}}
        acct = {"name": "u1", "base": "arm", "ephemeral": True,
                "adb_port": 16001, "qmp_port": 17001, "vnc_port": 18001,
                "data_image": "base_arm_data_offset_2.7.qcow2"}
        with mock.patch.object(qemu_proc, "arm_edk2_code",
                               return_value="/x/edk2.fd"), \
             mock.patch.object(qemu_proc, "qemu_bin",
                               side_effect=lambda t: t), \
             mock.patch.object(qemu_proc, "resolve_gpu_display",
                               return_value=([], [])):
            cmd = qemu_proc.qemu_command_arm(acct, cfg, interactive=False)
        vdb = [a for a in cmd if a.startswith("file=") and "id=vdb" in a]
        self.assertTrue(vdb, cmd)
        self.assertIn("base_arm_data_offset_2.7.qcow2", vdb[0])

    def test_no_offset_falls_back_to_the_bases_own_clean_data(self):
        from omnidroid import qemu_proc
        from pathlib import Path
        import tempfile
        tmp = tempfile.mkdtemp()
        Path(tmp, "base_arm_efivars.fd").write_bytes(b"E")
        cfg = {"images_dir": tmp, "qemu": {"smp": 4, "mem_mb": 4096},
               "bases": {"arm": _base()}}
        acct = {"name": "u1", "base": "arm", "ephemeral": True,
                "adb_port": 16001, "qmp_port": 17001, "vnc_port": 18001}
        with mock.patch.object(qemu_proc, "arm_edk2_code",
                               return_value="/x/edk2.fd"), \
             mock.patch.object(qemu_proc, "qemu_bin",
                               side_effect=lambda t: t), \
             mock.patch.object(qemu_proc, "resolve_gpu_display",
                               return_value=([], [])):
            cmd = qemu_proc.qemu_command_arm(acct, cfg, interactive=False)
        vdb = [a for a in cmd if a.startswith("file=") and "id=vdb" in a]
        self.assertIn(PRISTINE, vdb[0])


class ApkProbing(unittest.TestCase):
    def test_a_non_apk_returns_empty_fields_rather_than_raising(self):
        info = offsets.apk_version_info("/definitely/not/here.apk")
        self.assertEqual(info, {"package": None, "version_name": None,
                                "version_code": None})

    def test_a_suggested_name_falls_back_to_the_file_stem(self):
        self.assertEqual(
            offsets.suggest_offset_name("/tmp/arceus-2.731.944.apk",
                                        info={"version_name": None}),
            "arceus-2.731.944")


if __name__ == "__main__":
    unittest.main()
