import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import config, engine  # noqa: E402


def _write_run(tmp, name, pid, adb):
    d = tmp / "runtime" / name
    d.mkdir(parents=True, exist_ok=True)
    (d / "run.json").write_text(json.dumps(
        {"pid": pid, "adb_port": adb, "qmp_port": adb + 1000,
         "vnc_port": adb + 2000, "started": 1.0}))


def test_runtime_dir_is_under_data_dir(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    assert engine.runtime_dir("bob") == tmp_path / "runtime" / "bob"


def test_running_instances_lists_only_live_pids(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    _write_run(tmp_path, "alive", os.getpid(), 16001)   # this process = alive
    _write_run(tmp_path, "dead", 2, 16002)              # pid 2 = not ours/dead
    names = {i["name"] for i in engine.running_instances()}
    assert "alive" in names
    assert "dead" not in names


def test_spawn_records_ports_in_runtime_run_json(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    # running_pid reads runtime/<name>/run.json
    d = tmp_path / "runtime" / "x"
    d.mkdir(parents=True)
    (d / "run.json").write_text(json.dumps(
        {"pid": os.getpid(), "adb_port": 16005}))
    assert engine.running_pid("x") == os.getpid()
    # a name with no runtime dir is not running
    assert engine.running_pid("ghost") is None


def test_allocate_ports_reuses_freed_slots(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    cfg = {"qemu": {"adb_port_start": 16001, "qmp_port_start": 17001,
                    "vnc_port_start": 18001}}
    # no running instances -> index 0
    assert engine.allocate_ports(cfg) == (16001, 17001, 18001)
    # one live instance at index 0 -> next launch gets index 1
    _write_run(tmp_path, "alive", os.getpid(), 16001)
    assert engine.allocate_ports(cfg) == (16002, 17002, 18002)
    # a DEAD instance at index 1 does NOT reserve a slot -> still index 1
    _write_run(tmp_path, "dead", 2, 16002)
    assert engine.allocate_ports(cfg) == (16002, 17002, 18002)


def test_build_acct_reserves_port_slot_before_spawn(tmp_path, monkeypatch):
    """A reservation (live launcher pid, no QEMU spawned yet) must be visible
    to allocate_ports the same way a live QEMU instance is -- this is what
    closes the concurrent-`start` race: launcher A allocates+reserves index 0
    inside the lock, so launcher B (racing right behind it) sees index 0 taken
    and allocates index 1 instead. If the launcher dies before spawn_qemu
    overwrites the reservation with the real QEMU pid, the reservation is
    self-healing: its pid goes dead and running_instances() stops counting
    it, freeing the slot back up."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    cfg = {"qemu": {"adb_port_start": 16001, "qmp_port_start": 17001,
                    "vnc_port_start": 18001}}
    assert engine.allocate_ports(cfg) == (16001, 17001, 18001)
    # This process reserves index 0 (as build_acct would, under the lock).
    engine._reserve_ports("a", 16001, 17001, 18001)
    assert engine.allocate_ports(cfg) == (16002, 17002, 18002)
    # Simulate the launcher dying before spawn_qemu ever overwrote run.json:
    # a dead pid must not keep the slot reserved.
    _write_run(tmp_path, "a", 2, 16001)
    assert engine.allocate_ports(cfg) == (16001, 17001, 18001)


def test_launch_lock_is_reentrant_safe_serial(tmp_path, monkeypatch):
    """Basic smoke test: acquiring and releasing the launch lock twice in a
    row (as two sequential `start` launches would) doesn't deadlock or
    error."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    with engine._launch_lock():
        pass
    with engine._launch_lock():
        pass
