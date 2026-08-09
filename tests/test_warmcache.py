#!/usr/bin/env python3
"""The warm-restore cache key.

    python3 -m pytest tests/test_warmcache.py -q

Invalidation in this design is a CONSEQUENCE of the key, not separate
bookkeeping: a base update, a new APK/offset, a mode change, a resize, or a
QEMU upgrade must each produce a different key so the stale entry is simply
never found. These tests are what keeps that property true.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import warmcache  # noqa: E402


BASE = dict(arch="arm64", base_tag="arm", base_version=3, offset="patched",
            mode_name="playable", mem_mb=8192, smp=6, machine="virt",
            accel="hvf", qemu_version="11.0.2")


class TheCacheKey(unittest.TestCase):
    def test_same_inputs_give_the_same_key(self):
        self.assertEqual(warmcache.cache_key(**BASE),
                         warmcache.cache_key(**BASE))

    def test_every_field_changes_the_key(self):
        # If any of these stopped mattering, a stale entry would be restored
        # against a machine it does not describe.
        changes = dict(arch="x86_64", base_tag="x86", base_version=4,
                       offset="arceus", mode_name="farming", mem_mb=4096,
                       smp=4, machine="q35", accel="kvm",
                       qemu_version="11.1.0")
        for field, value in changes.items():
            with self.subTest(field=field):
                other = dict(BASE, **{field: value})
                self.assertNotEqual(warmcache.cache_key(**BASE),
                                    warmcache.cache_key(**other), field)

    def test_key_is_filesystem_safe_and_short(self):
        key = warmcache.cache_key(**BASE)
        self.assertEqual(len(key), 24)
        self.assertTrue(all(c in "0123456789abcdef" for c in key))

    def test_numeric_fields_compare_by_value_not_text(self):
        # "8192" and 8192 must not be two different cache entries.
        self.assertEqual(warmcache.cache_key(**dict(BASE, mem_mb=8192)),
                         warmcache.cache_key(**dict(BASE, mem_mb="8192")))


if __name__ == "__main__":
    unittest.main()
