"""The warm POOL: keys, claiming, adoption.

    python3 -m pytest tests/test_pool.py -q

The pool exists because Windows cannot have the warm-boot CACHE at all --
QEMU/WHPX refuses to serialise a guest, measured and final (MODES.md, "Boot
time"). A pool never serialises anything, so the questions that matter here
are not about state files. They are:

  * does a launch compute the SAME key as the manager that filled the pool?
    (if not, the pool never hits and nobody finds out -- it just stays slow)
  * can two launches be handed the same instance? (that is two accounts on
    one guest, i.e. the wrong cookie in a live game)
  * does an adopted instance still read as ALIVE? (identity is the trap: the
    QEMU process is named after the SLOT and cannot be renamed)
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import pytest  # noqa: E402

from omnidroid import pool  # noqa: E402


BASE_KEY = dict(arch="x86", base_tag="x86", base_version=3, offset="arceus",
                offset_image_stat=(996016128, 1755000000), accel="whpx")


def _key(**spec):
    return pool.slot_key(spec=spec, **BASE_KEY)


def test_key_ignores_how_the_spec_was_spelled():
    # A spec that omits a knob and one that sets it to None describe the same
    # machine. If these hashed differently, a pool filled by `pool start`
    # (which fills every key) would never match a plain `start` (which does
    # not) -- the pool would silently never hit.
    assert _key(mode="gaming") == _key(mode="gaming", mem=None, quality=None)


def test_key_separates_machines_that_really_differ():
    a = _key(mode="gaming", mem=4096, smp=4)
    assert a != _key(mode="gaming", mem=2048, smp=4)
    assert a != _key(mode="gaming", mem=4096, smp=2)
    assert a != _key(mode="farming", mem=4096, smp=4)
    assert a != _key(mode="gaming", mem=4096, smp=4, gpu="headless")
    assert a != _key(mode="gaming", mem=4096, smp=4, panel="640x480")


def test_key_follows_the_offset_IMAGE_not_just_its_name():
    # `offset delete X` + `offset create X <other apk>` reuses the name for a
    # different Roblox build. A name-only key would hand a launch a slot
    # running the OLD client, which is the failure warmcache.cache_key already
    # documents -- and it is worse here, because the slot is live.
    other = dict(BASE_KEY, offset_image_stat=(996016128, 1755999999))
    assert (pool.slot_key(spec={"mode": "gaming"}, **BASE_KEY)
            != pool.slot_key(spec={"mode": "gaming"}, **other))


def test_guest_display_tuple_and_list_hash_the_same():
    # The key is a hash of JSON, and JSON has no tuples: a mode carrying
    # (480, 270, 80) and the same spec after a round trip through pool.json
    # must not be two different machines.
    assert (pool.slot_key(spec={"guest_display": (480, 270, 80)}, **BASE_KEY)
            == pool.slot_key(spec={"guest_display": [480, 270, 80]},
                             **BASE_KEY))


# ------------------------------------------------------------ claiming


def _slot(tmp, name, key, *, state="ready", pid=None, ready_at=1.0,
          ports=(16001, 17001, 18001)):
    d = tmp / "runtime" / name
    d.mkdir(parents=True, exist_ok=True)
    (d / "pool.json").write_text(json.dumps(
        {"key": key, "state": state, "ready_at": ready_at, "spec": {}}))
    (d / "run.json").write_text(json.dumps(
        {"pid": pid or os.getpid(), "adb_port": ports[0],
         "qmp_port": ports[1], "vnc_port": ports[2], "base": "x86",
         "offset": "arceus", "data_image": "d.qcow2",
         # No `identity` -> instance_live falls back to pid-only, which is
         # what makes this testable without a real QEMU.
         "mode": "gaming"}))
    return d


def test_a_slot_is_claimed_exactly_once(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")

    first = pool.claim("K")
    second = pool.claim("K")

    assert first is not None and first["slot"] == "_pool0"
    # THE failure this guards: two launches on one guest means the second
    # account's cookie lands in the first account's game.
    assert second is None


def test_claim_ignores_slots_with_another_key(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "OTHER")
    assert pool.claim("K") is None


def test_claim_hands_out_the_oldest_first(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K", ready_at=200.0, ports=(16001, 17001, 18001))
    _slot(tmp_path, "_pool1", "K", ready_at=100.0, ports=(16002, 17002, 18002))
    assert pool.claim("K")["slot"] == "_pool1"


def test_release_puts_a_slot_back(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")
    taken = pool.claim("K")
    pool.release(taken["slot"], "guest not answering")
    assert pool.claim("K") is not None


def test_a_dead_slot_is_never_handed_out(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    # pid 1 is not this test's process; instance_live's pid check fails.
    d = _slot(tmp_path, "_pool0", "K")
    run = json.loads((d / "run.json").read_text())
    run["pid"] = 999999999
    (d / "run.json").write_text(json.dumps(run))
    assert pool.claim("K") is None


# ------------------------------------------------------------- adoption


def test_adoption_keeps_the_slots_identity(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    d = _slot(tmp_path, "_pool0", "K")
    run = json.loads((d / "run.json").read_text())
    run["identity"] = "omni-_pool0"
    (d / "run.json").write_text(json.dumps(run))

    adopted = pool.adopt_run_json("_pool0", "berat")

    written = json.loads(
        (tmp_path / "runtime" / "berat" / "run.json").read_text())
    # The QEMU process was named at spawn and cannot be renamed. instance_live
    # compares the RECORDED identity against QMP query-name, so rewriting this
    # to "omni-berat" would make a perfectly healthy adopted instance read as
    # dead -- and `start` would then try to boot a second one on its ports.
    assert written["identity"] == "omni-_pool0"
    assert written["pool_slot"] == "_pool0"
    assert written["adb_port"] == adopted["adb_port"] == 16001


def test_adoption_leaves_the_slot_directory_alone(tmp_path, monkeypatch):
    # Windows will not move a directory whose files are open, and QEMU holds
    # qemu.log open for the life of the instance. Adoption must therefore be a
    # copy; the slot dir is cleaned up later, by sweep(), once the process is
    # gone.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    d = _slot(tmp_path, "_pool0", "K")
    (d / "qemu.log").write_text("...")
    pool.adopt_run_json("_pool0", "berat")
    assert (d / "run.json").exists() and (d / "qemu.log").exists()


def test_sweep_removes_dead_slots_but_not_live_ones(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")                       # live (our pid)
    dead = _slot(tmp_path, "_pool1", "K", ports=(16002, 17002, 18002))
    run = json.loads((dead / "run.json").read_text())
    run["pid"] = 999999999
    (dead / "run.json").write_text(json.dumps(run))

    removed = pool.sweep()

    assert removed == ["_pool1"]
    assert (tmp_path / "runtime" / "_pool0").exists()


def test_sweep_does_not_kill_a_slot_that_is_still_booting(tmp_path,
                                                          monkeypatch):
    # A slot is registered (pool.json written) BEFORE its QEMU is spawned, so
    # for a few seconds it is live-looking-dead. Sweeping it then would delete
    # the record of a boot that is about to succeed.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    d = tmp_path / "runtime" / "_pool0"
    d.mkdir(parents=True)
    (d / "pool.json").write_text(json.dumps({"key": "K", "state": "booting"}))
    assert pool.sweep() == []
    assert d.exists()


def test_free_slot_name_skips_what_exists(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")
    assert pool.free_slot_name() == "_pool1"
    assert pool.free_slot_name(taken={"_pool1"}) == "_pool2"


def test_an_account_cannot_be_named_like_a_slot():
    # sweep() deletes dead `_pool*` directories, so an account with that name
    # is an account whose runtime the pool GCs out from under it.
    from omnidroid import engine
    with pytest.raises(SystemExit):
        engine.validate_instance_name("_pool0")
    # ...and the pool itself is still allowed its own prefix.
    assert engine.validate_instance_name("_pool0", allow_pool=True) == "_pool0"


def test_a_name_that_is_not_a_directory_is_refused():
    # Adoption writes runtime/<name>/run.json BEFORE build_acct's own check is
    # reached, so the validation has to happen earlier than the caller that
    # historically owned it.
    from omnidroid import engine
    for bad in ("../evil", "a/b", "", "a b"):
        with pytest.raises(SystemExit):
            engine.validate_instance_name(bad)


def test_summary_counts_what_it_says(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")
    _slot(tmp_path, "_pool1", "K", state="booting",
          ports=(16002, 17002, 18002))
    s = pool.summary("K")
    assert s["total"] == 2 and s["ready"] == 1 and s["booting"] == 1
    assert s["ready_matching"] == 1
