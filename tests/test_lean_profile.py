#!/usr/bin/env python3
"""The lean (low-RAM) profile: baked properties and the runtime trim list.

    python3 tests/test_lean_profile.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import lean  # noqa: E402


class BakedProps(unittest.TestCase):
    """The profile is GATED OFF, for two bisected reasons:
    ro.config.low_ram=true alone bricks the arm64 base (SystemUI crash-loop
    -> RescueParty -> recovery), and the complementary safe subset boots but
    saves nothing measurable. baked_props() returns nothing until a caller
    opts in, because strip-base writes to an image the whole fleet is backed
    by."""

    def test_default_is_empty_while_unverified(self):
        self.assertFalse(lean.PROFILE_VERIFIED)
        self.assertEqual(lean.baked_props(), {})

    def test_opt_in_returns_the_experimental_set(self):
        self.assertEqual(
            lean.baked_props(include_unverified=True)["ro.config.low_ram"],
            "true")

    def test_evidence_is_recorded_for_whoever_opts_in(self):
        self.assertIn("RescueParty", lean.PROFILE_EVIDENCE)
        self.assertIn("ro.config.low_ram", lean.PROFILE_EVIDENCE)

    def test_tiers_do_not_collide(self):
        """Each tier owns its keys; a silent overwrite between tiers would
        mean one tier's carefully chosen value is dead text."""
        tiers = (lean.LOW_RAM_PROPS, lean.DALVIK_LOW_RAM_PROPS,
                 lean.LMKD_PROPS, lean.HWUI_LOW_RAM_PROPS, lean.DEXOPT_PROPS)
        seen = set()
        for tier in tiers:
            clash = seen & set(tier)
            self.assertFalse(clash, f"key defined in two tiers: {clash}")
            seen |= set(tier)
        self.assertEqual(
            len(seen), len(lean.baked_props(include_unverified=True)))

    def test_every_value_is_a_string(self):
        """build.prop is text; a stray int would render as `True`/`1` and
        quietly mean something different to init."""
        for k, v in lean.baked_props(include_unverified=True).items():
            self.assertIsInstance(v, str, k)

    def test_build_prop_lines_start_on_a_fresh_line(self):
        """The file we append to may not end in a newline; without a leading
        one our first key would be glued onto the image's last line."""
        lines = lean.build_prop_lines(
            lean.baked_props(include_unverified=True))
        self.assertTrue(lines.startswith("\n"))
        self.assertTrue(lines.endswith("\n"))


class MergeBuildProp(unittest.TestCase):
    """Merging must REPLACE, not append.

    init refuses to redefine a ro.* property once set, so the first
    definition in the file wins. An appended duplicate is silently ignored,
    which is the failure that looks like success."""

    def test_existing_key_is_removed_not_duplicated(self):
        out = lean.merge_build_prop(
            "ro.config.low_ram=false\nro.build.id=X\n",
            {"ro.config.low_ram": "true"})
        self.assertNotIn("ro.config.low_ram=false", out)
        self.assertEqual(out.count("ro.config.low_ram="), 1)
        self.assertIn("ro.config.low_ram=true", out)

    def test_unrelated_keys_and_comments_survive(self):
        out = lean.merge_build_prop(
            "# a comment\nro.build.id=X\n\nro.product.model=Y\n",
            {"ro.config.low_ram": "true"})
        for keep in ("# a comment", "ro.build.id=X", "ro.product.model=Y"):
            self.assertIn(keep, out)

    def test_commented_out_assignment_is_not_treated_as_a_key(self):
        out = lean.merge_build_prop("#ro.config.low_ram=false\n",
                                    {"ro.config.low_ram": "true"})
        self.assertIn("#ro.config.low_ram=false", out)

    def test_whitespace_around_key_is_handled(self):
        out = lean.merge_build_prop("  ro.config.low_ram = false \n",
                                    {"ro.config.low_ram": "true"})
        self.assertNotIn("false", out)

    def test_rebaking_is_idempotent(self):
        """A base gets re-baked whenever it is rebuilt. Without stripping our
        own marker, every pass would leave an orphaned header behind and
        append a fresh one — unbounded growth across the base's lifetime."""
        props = lean.baked_props(include_unverified=True)
        once = lean.merge_build_prop("ro.build.id=X\n", props)
        twice = lean.merge_build_prop(once, props)
        self.assertEqual(once, twice)
        self.assertEqual(twice.count(lean.PROFILE_MARKER), 1)

    def test_empty_input_still_yields_every_property(self):
        out = lean.merge_build_prop("", {"a.b": "1", "c.d": "2"})
        self.assertIn("a.b=1", out)
        self.assertIn("c.d=2", out)


class TrimList(unittest.TestCase):
    def test_keep_always_cannot_be_trimmed(self):
        """The guard rail: naming a must-keep package must not remove it,
        even by explicit request."""
        got = lean.trim_packages(extra=lean.KEEP_ALWAYS)
        for pkg in lean.KEEP_ALWAYS:
            self.assertNotIn(pkg, got)

    def test_systemui_and_the_game_are_never_trimmed(self):
        got = lean.trim_packages()
        self.assertNotIn("com.android.systemui", got)
        self.assertNotIn("com.roblox.client", got)

    def test_extra_packages_are_appended_and_deduped(self):
        got = lean.trim_packages(extra=("com.example.x", "com.example.x"))
        self.assertEqual(got.count("com.example.x"), 1)

    def test_no_duplicates_in_the_shipped_list(self):
        self.assertEqual(len(lean.FARMING_TRIM_PACKAGES),
                         len(set(lean.FARMING_TRIM_PACKAGES)))


class Display(unittest.TestCase):
    def test_none_leaves_the_native_resolution_alone(self):
        self.assertEqual(lean.display_args(lean.NATIVE_DISPLAY), [])

    def test_farming_display_sets_size_and_density(self):
        args = lean.display_args(lean.FARMING_DISPLAY)
        self.assertEqual(args[0], ["shell", "wm", "size", "480x270"])
        self.assertEqual(args[1], ["shell", "wm", "density", "80"])


class ProvisionTrimGap(unittest.TestCase):
    """Tier 1 must be in the squeeze, not only in first-boot provisioning.

    `lockdown_and_trim` runs from `provision_settings`, which only fires on a
    first boot — and build_acct() hands the production path a handle with
    first_boot_done already True. So on an ephemeral instance that list never
    ran, and every package in it was found resident on a booted farming
    instance. Disabling them from the squeeze measured -70 MB guest-used with
    the game running (2026-08-05)."""

    def test_provision_tier_is_included_in_the_squeeze_list(self):
        got = set(lean.trim_packages())
        for pkg in lean.PROVISION_TRIM_PACKAGES:
            self.assertIn(pkg, got)

    def test_the_two_tiers_do_not_overlap(self):
        self.assertFalse(set(lean.PROVISION_TRIM_PACKAGES)
                         & set(lean.FARMING_TRIM_PACKAGES))

    def test_keep_always_still_wins_over_tier_one(self):
        self.assertFalse(set(lean.trim_packages()) & set(lean.KEEP_ALWAYS))

    def test_measured_offenders_are_covered(self):
        """The four found resident, by RSS, on a live instance."""
        got = set(lean.trim_packages())
        for pkg in ("com.android.deskclock", "org.lineageos.updater",
                    "org.lineageos.lineageparts",
                    "com.android.permissioncontroller"):
            self.assertIn(pkg, got)


class StripBaseGate(unittest.TestCase):
    """strip-base must REFUSE while the profile is unverified.

    Not defensive style for its own sake: strip-base writes to a base image
    that every account's COW overlay is backed by, and the profile it would
    write puts the guest into recovery. An accidental run is a fleet-wide
    outage, not one broken instance."""

    def test_refuses_without_the_force_flag(self):
        import types
        from omnidroid import engine as omni
        with self.assertRaises(SystemExit):
            omni.cmd_strip_base(types.SimpleNamespace(
                base=None, out=None, in_place=False,
                force_unverified=False, json=False))

    def test_refusal_names_the_evidence(self):
        """Whoever hits this refusal must learn WHY and what to do next,
        without going and re-running the experiment themselves."""
        ev = lean.PROFILE_EVIDENCE.lower()
        for token in ("ro.config.low_ram", "rescueparty", "bisect",
                      "dev base", "saved nothing"):
            self.assertIn(token, ev)


class BisectFindings(unittest.TestCase):
    """The bisect result, locked in so it is not re-litigated by name.

    ro.config.low_ram=true was tested ALONE and bricks the arm64 base
    (SystemUI crash-loop -> RescueParty -> recovery). The complementary
    8-property subset boots but saved nothing (1069 -> 1170 MB guest-used).
    Both facts matter: the first says do not bake it, the second says there
    is no consolation prize in baking the rest."""

    def test_low_ram_is_recorded_as_boot_breaking(self):
        self.assertIn("ro.config.low_ram", lean.BOOT_BREAKING_PROPS)

    def test_boot_breaking_props_are_never_in_the_verified_set(self):
        for pkg in lean.BOOT_BREAKING_PROPS:
            self.assertNotIn(pkg, lean.BOOT_VERIFIED_PROPS)

    def test_verified_subset_is_a_subset_of_the_full_profile(self):
        full = lean.baked_props(include_unverified=True)
        for k, v in lean.BOOT_VERIFIED_PROPS.items():
            self.assertEqual(full.get(k), v, k)

    def test_profile_stays_gated_off(self):
        """It boots-or-bricks aside, it saves nothing - so default is off."""
        self.assertFalse(lean.PROFILE_VERIFIED)
        self.assertEqual(lean.baked_props(), {})


if __name__ == "__main__":
    unittest.main()
