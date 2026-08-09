#!/usr/bin/env python3
"""The warm-restore cache key.

    python3 -m pytest tests/test_warmcache.py -q

Invalidation in this design is a CONSEQUENCE of the key, not separate
bookkeeping: a base update, a new APK/offset, a mode change, a resize, or a
QEMU upgrade must each produce a different key so the stale entry is simply
never found. These tests are what keeps that property true.
"""
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

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


def _make_entry(tmp, key, qemu_version="11.0.2", missing=()):
    """Build a complete-looking entry on disk; `missing` omits files."""
    e = warmcache.entry_path(tmp, key)
    e.mkdir(parents=True, exist_ok=True)
    for name in warmcache.REQUIRED_FILES:
        if name in missing or name == warmcache.META_NAME:
            continue
        (e / name).write_bytes(b"x")
    if warmcache.META_NAME not in missing:
        (e / warmcache.META_NAME).write_text(json.dumps(
            {"key": key, "qemu_version": qemu_version, "mem_mb": 8192,
             "smp": 6, "last_used": 0}))
    return e


class EntryLookup(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.key = warmcache.cache_key(**BASE)

    def test_complete_entry_is_found(self):
        e = _make_entry(self.tmp, self.key)
        self.assertEqual(warmcache.lookup(self.tmp, self.key, "11.0.2"), e)

    def test_missing_entry_is_a_miss_not_an_error(self):
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_each_missing_file_is_a_miss(self):
        for name in warmcache.REQUIRED_FILES:
            with self.subTest(missing=name):
                tmp = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
                _make_entry(tmp, self.key, missing=(name,))
                self.assertIsNone(warmcache.lookup(tmp, self.key, "11.0.2"))

    def test_qemu_version_mismatch_is_a_miss(self):
        # The migration stream format is tied to the QEMU build that wrote it.
        _make_entry(self.tmp, self.key, qemu_version="11.0.2")
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.1.0"))

    def test_corrupt_meta_is_a_miss_not_a_crash(self):
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.META_NAME).write_text("{not json")
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_meta_key_must_match_the_directory_key(self):
        # Guards against a hand-copied or half-renamed entry.
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.META_NAME).write_text(json.dumps(
            {"key": "somethingelse", "qemu_version": "11.0.2"}))
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_each_zero_byte_file_is_a_miss(self):
        # A zero-byte state/qcow2/efivars file is a truncated write, not a
        # usable entry -- handing it to QEMU fails in a way nobody traces
        # back to the cache.
        for name in warmcache.REQUIRED_FILES:
            with self.subTest(zero_byte=name):
                tmp = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
                e = _make_entry(tmp, self.key)
                (e / name).write_bytes(b"")
                self.assertIsNone(warmcache.lookup(tmp, self.key, "11.0.2"))

    def test_state_as_a_directory_is_a_miss(self):
        # REQUIRED_FILES entries must be regular files; a directory that
        # happens to share the name must not be accepted as the file.
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.STATE_NAME).unlink()
        (e / warmcache.STATE_NAME).mkdir()
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_missing_qemu_version_in_meta_is_a_miss_even_with_none_argument(self):
        # meta.get(...) != qemu_version must not pass spuriously when both
        # sides are None -- an entry with no recorded QEMU version is never
        # valid, regardless of what the caller passes.
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.META_NAME).write_text(json.dumps({"key": self.key}))
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, None))


class ReadMetaShape(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def test_read_meta_rejects_a_json_list(self):
        # Valid JSON but the wrong shape: read_meta must treat this as
        # absent rather than returning something callers would .get() into
        # an AttributeError.
        entry = self.tmp / "entry"
        entry.mkdir()
        (entry / warmcache.META_NAME).write_text(json.dumps(["not", "a", "dict"]))
        self.assertIsNone(warmcache.read_meta(entry))


if __name__ == "__main__":
    unittest.main()
