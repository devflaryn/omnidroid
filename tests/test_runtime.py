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
