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
import time

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


# A pid the OS will not have handed to anybody: used wherever a test needs a
# process that is definitely gone (a manager that exited, a QEMU that died).
DEAD_PID = 999999999


def _slot(tmp, name, key, *, state="ready", pid=None, ready_at=1.0,
          ports=(16001, 17001, 18001), owner_pid=None, meta=None):
    d = tmp / "runtime" / name
    d.mkdir(parents=True, exist_ok=True)
    rec = {"key": key, "state": state, "ready_at": ready_at, "spec": {}}
    if owner_pid is not None:
        rec["owner_pid"] = owner_pid
    rec.update(meta or {})
    (d / "pool.json").write_text(json.dumps(rec))
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


# ------------------------------------------------------ surviving a restart
#
# A slot is a DETACHED QEMU: it outlives the manager that booted it, and it
# outlives the whole app. The question these ask is whether the pool can still
# find it afterwards -- because if it cannot, the user pays a 47-190 s boot for
# a guest that is sitting right there, and the pool's entire reason to exist
# (0.082 s) is gone.


def test_a_live_slot_is_re_adopted_after_the_manager_exits(tmp_path,
                                                           monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    # The manager that booted this slot is gone (dead owner_pid); its QEMU is
    # not (run.json carries a live pid). Nothing is held in memory, so the
    # restarted manager must re-derive all of this from disk.
    _slot(tmp_path, "_pool0", "K", owner_pid=DEAD_PID)

    s = pool.slot_state(tmp_path / "runtime" / "_pool0")
    assert s["live"] is True
    assert s["orphaned"] is False          # it reached `ready` before dying
    assert [x["slot"] for x in pool.ready_slots("K")] == ["_pool0"]
    assert pool.sweep() == []              # and it is NOT a corpse
    assert pool.claim("K")["slot"] == "_pool0"


def test_a_slot_interrupted_mid_boot_is_reported_as_orphaned(tmp_path,
                                                             monkeypatch):
    # The nastiest durability case: the app died between "register the slot"
    # and "write ready". The guest may well be up, but no process is left to
    # finish the post-boot pipeline, so it can never be handed out -- and
    # because it is LIVE it can never be swept either. Left unreported it sits
    # there holding ~2.2 GB and its `_pool0` name forever.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K", state="booting", owner_pid=DEAD_PID)

    s = pool.slot_state(tmp_path / "runtime" / "_pool0")
    assert s["live"] is True and s["orphaned"] is True
    assert pool.summary("K")["orphaned"] == 1
    # Surfaced, never deleted: it is a live guest.
    assert pool.sweep() == []
    assert (tmp_path / "runtime" / "_pool0").exists()


def test_a_slot_that_never_spawned_is_swept_once_its_booter_is_gone(
        tmp_path, monkeypatch):
    # pool.json written, QEMU never spawned, booting process gone. Nothing
    # will ever spawn it, and leaving the directory holds `_pool0` against
    # free_slot_name() across every future restart.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    d = tmp_path / "runtime" / "_pool0"
    d.mkdir(parents=True)
    (d / "pool.json").write_text(json.dumps(
        {"key": "K", "state": "booting", "owner_pid": DEAD_PID}))

    assert pool.sweep() == ["_pool0"]
    assert pool.free_slot_name() == "_pool0"


def test_an_adopted_live_slot_is_never_swept(tmp_path, monkeypatch):
    # Adoption COPIES run.json, so this directory is still the one QEMU holds
    # qemu.log open in. Sweeping it pulls the log out from under a guest an
    # account is playing on -- and on Windows would not even succeed.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K")
    pool.claim("K")
    pool.mark_adopted("_pool0", "berat")

    assert pool.slot_state(tmp_path / "runtime" / "_pool0")["live"] is True
    assert pool.sweep() == []
    assert (tmp_path / "runtime" / "_pool0").exists()
    # ...and it is not handed to a second launch either.
    assert pool.claim("K") is None


def test_an_adopted_slot_whose_qemu_died_is_swept(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    d = _slot(tmp_path, "_pool0", "K", pid=DEAD_PID)
    (d / "adopted.json").write_text(json.dumps({"account": "berat"}))
    assert pool.sweep() == ["_pool0"]


# ----------------------------------------------------------- pool policy


def test_persistent_defaults_true_and_round_trips(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    pool.write_pool({"size": 2, "key": "K", "spec": {"mode": "gaming"}})
    # Absent must mean True, or the first manager to run after this change
    # reads every pool already on disk as "fill once, never top up".
    assert pool.read_pool()["persistent"] is True
    assert pool.desired() == {"size": 2, "key": "K",
                              "spec": {"mode": "gaming"}, "persistent": True}

    pool.write_pool({"size": 2, "key": "K", "spec": {}, "persistent": False})
    assert pool.read_pool()["persistent"] is False
    assert pool.desired()["persistent"] is False


def test_a_pool_record_written_before_the_flag_existed_still_refills(
        tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    (tmp_path / "runtime").mkdir(parents=True)
    (tmp_path / "runtime" / "pool.json").write_text(
        json.dumps({"size": 1, "key": "K", "spec": {}}))
    assert pool.desired()["persistent"] is True


def test_desired_is_zero_and_harmless_when_there_is_no_pool(tmp_path,
                                                            monkeypatch):
    # size 0 is the manager's exit condition, so a missing or hand-mangled
    # pool.json has to land there rather than raise inside the manager loop.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    assert pool.desired()["size"] == 0
    pool.write_pool({"size": "banana", "spec": "not-a-dict"})
    assert pool.desired()["size"] == 0
    assert pool.desired()["spec"] == {}


def test_stale_slots_respects_the_age_threshold(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K", ready_at=1000.0)          # 500 s old
    _slot(tmp_path, "_pool1", "K", ready_at=1400.0,          # 100 s old
          ports=(16002, 17002, 18002))
    now = 1500.0

    assert [s["slot"] for s in pool.stale_slots(300, now=now)] == ["_pool0"]
    assert pool.stale_slots(600, now=now) == []
    # Oldest first, so a caller recycling one per tick takes the worst first.
    assert [s["slot"] for s in pool.stale_slots(50, now=now)] == ["_pool0",
                                                                 "_pool1"]


def test_stale_slots_leaves_dead_and_adopted_slots_out_of_it(tmp_path,
                                                             monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    dead = _slot(tmp_path, "_pool0", "K", ready_at=1.0, pid=DEAD_PID)
    assert dead.exists()
    _slot(tmp_path, "_pool1", "K", ready_at=1.0, ports=(16002, 17002, 18002))
    pool.claim("K")
    pool.mark_adopted("_pool1", "berat")

    # A dead slot is sweep()'s business, not a recycle candidate; an adopted
    # one is somebody's live game and "old" is no reason to touch it.
    assert pool.stale_slots(10, now=1000.0) == []


def test_a_slot_with_no_recorded_age_is_never_stale(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K", ready_at=None)
    assert pool.slot_state(tmp_path / "runtime" / "_pool0")["age_s"] is None
    assert pool.stale_slots(0, now=1e9) == []


def test_a_booting_slot_ages_from_when_it_started(tmp_path, monkeypatch):
    # A boot wedged for an hour is exactly what a caller wants to find, and it
    # is the one slot with no ready_at.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _slot(tmp_path, "_pool0", "K", state="booting", ready_at=None,
          meta={"started": 900.0})
    assert pool.slot_state(tmp_path / "runtime" / "_pool0",
                           now=1000.0)["age_s"] == 100.0


# --------------------------------------------------------- the manager


def test_a_recycled_manager_pid_does_not_pass_for_a_running_manager(
        tmp_path, monkeypatch):
    # The old check was pid_alive(manager_pid) alone. After a restart the OS
    # can hand that pid to anything, and on such a host the app decides a
    # manager is already running, never spawns one, and the pool silently
    # never refills again.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    pool.write_pool({"size": 1, "key": "K", "spec": {}})
    assert pool.manager_alive() is False          # no beacon at all

    pool.manager_heartbeat()                      # this process IS a manager
    assert pool.manager_alive() is True
    # Same live pid, but the beacon has gone cold: a manager that was killed.
    assert pool.manager_alive(stale_s=60, now=time.time() + 600) is False


def test_stopping_the_pool_takes_the_beacon_with_it(tmp_path, monkeypatch):
    # Otherwise the next manager_alive() answers True for a manager that is
    # about to exit precisely because the config went away.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    pool.write_pool({"size": 1, "key": "K", "spec": {}})
    pool.manager_heartbeat()
    pool.clear_pool()
    assert pool.manager_alive() is False
    assert pool.read_pool() is None


def test_a_heartbeat_after_pool_stop_does_not_resurrect_the_pool(tmp_path,
                                                                 monkeypatch):
    # `pool stop` clears the config so the manager exits at its next tick. A
    # heartbeat landing microseconds later must not put the pool back --
    # which is the whole reason the beacon is a separate file from pool.json.
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    pool.write_pool({"size": 1, "key": "K", "spec": {}})
    pool.clear_pool()
    pool.manager_heartbeat()
    assert pool.read_pool() is None
    assert pool.desired()["size"] == 0
