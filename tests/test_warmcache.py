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

    def test_separator_injection_does_not_cause_collision(self):
        # Pipe characters in string fields must not shift the boundary.
        # These two shapes should produce DIFFERENT keys.
        key1 = warmcache.cache_key(arch='arm64', base_tag='arm', base_version=3,
                                   offset='a|b', mode_name='c',
                                   mem_mb=8192, smp=6, machine='virt',
                                   accel='hvf', qemu_version='11.0.2')
        key2 = warmcache.cache_key(arch='arm64', base_tag='arm', base_version=3,
                                   offset='a', mode_name='b|c',
                                   mem_mb=8192, smp=6, machine='virt',
                                   accel='hvf', qemu_version='11.0.2')
        self.assertNotEqual(key1, key2)

    def test_non_numeric_mem_mb_does_not_raise(self):
        # Bad input should produce a key, not raise ValueError.
        key = warmcache.cache_key(**dict(BASE, mem_mb="not-a-number"))
        self.assertEqual(len(key), 24)
        self.assertTrue(all(c in "0123456789abcdef" for c in key))
        # The bad key should differ from the valid one.
        valid_key = warmcache.cache_key(**dict(BASE, mem_mb=8192))
        self.assertNotEqual(key, valid_key)

    def test_none_smp_does_not_raise(self):
        # None should produce a key, not raise TypeError.
        key = warmcache.cache_key(**dict(BASE, smp=None))
        self.assertEqual(len(key), 24)
        self.assertTrue(all(c in "0123456789abcdef" for c in key))
        # The bad key should differ from the valid one.
        valid_key = warmcache.cache_key(**dict(BASE, smp=6))
        self.assertNotEqual(key, valid_key)


if __name__ == "__main__":
    unittest.main()
