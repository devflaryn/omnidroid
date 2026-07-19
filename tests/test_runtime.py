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
