# tests/test_verified_liveness.py
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from omnidroid import runtime  # noqa: E402

def test_expected_identity_matches_qemu_name_flag():
    # The -name flag both qemu_command paths emit is f"omni-{name}".
    assert runtime.expected_identity({"name": "acc0"}) == "omni-acc0"

def test_run_json_records_identity(tmp_path, monkeypatch):
    # spawn_qemu writes run.json with an identity field equal to the -name token.
    from omnidroid import qemu_proc
    monkeypatch.setattr(qemu_proc, "check_accel", lambda: None)
    monkeypatch.setattr(qemu_proc, "qemu_command", lambda *a, **k: ["true"])
    monkeypatch.setattr(runtime, "runtime_dir", lambda name: tmp_path / name)
    # qemu_proc.runtime_dir is imported lazily from runtime; patch there too.
    monkeypatch.setattr("omnidroid.runtime.runtime_dir",
                        lambda name: tmp_path / name)
    acct = {"name": "acc0", "base": "arm", "adb_port": 6000,
            "qmp_port": 7000, "vnc_port": 18001}
    qemu_proc.spawn_qemu(acct, {"qemu": {}}, interactive=False)
    rj = json.loads((tmp_path / "acc0" / "run.json").read_text())
    assert rj["identity"] == "omni-acc0"

import socket
import threading

def test_cmdline_has_token_true(tmp_path, monkeypatch):
    # Simulate /proc/<pid>/cmdline as a NUL-joined arg vector containing the token.
    proc_dir = tmp_path / "12345"
    proc_dir.mkdir()
    (proc_dir / "cmdline").write_bytes(b"qemu\x00-name\x00omni-acc0\x00")
    monkeypatch.setattr(runtime, "_PROC", tmp_path)
    assert runtime._cmdline_has_token(12345, "omni-acc0") is True
    assert runtime._cmdline_has_token(12345, "omni-other") is False

def test_cmdline_has_token_no_proc(monkeypatch):
    monkeypatch.setattr(runtime, "_PROC", pathlib.Path("/nonexistent-proc"))
    assert runtime._cmdline_has_token(1, "omni-acc0") is False

def _fake_qmp_server(name):
    # Minimal QMP: greeting, accept qmp_capabilities, answer query-name.
    srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
    port = srv.getsockname()[1]
    def serve():
        c, _ = srv.accept()
        f = c.makefile("rw")
        f.write('{"QMP":{"version":{}}}\n'); f.flush()
        f.readline()                       # qmp_capabilities
        f.write('{"return":{}}\n'); f.flush()
        f.readline()                       # query-name
        f.write(json.dumps({"return": {"name": name}}) + "\n"); f.flush()
        c.close(); srv.close()
    threading.Thread(target=serve, daemon=True).start()
    return port

def test_qmp_name_reads_guest_name():
    port = _fake_qmp_server("omni-acc0")
    assert runtime._qmp_name(port) == "omni-acc0"

def test_qmp_name_none_when_nobody_listens():
    # An almost-certainly-closed port returns None fast.
    assert runtime._qmp_name(1) is None

import subprocess

def test_instance_live_rejects_recycled_pid(monkeypatch):
    # A live but UNRELATED process (a sleep). pid is alive, but neither the
    # cmdline token nor QMP identity match -> not our instance.
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 1,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token", lambda pid, tok: False)
        monkeypatch.setattr(runtime, "_qmp_name", lambda port, timeout=0.25: None)
        assert runtime.instance_live(rec) is False
    finally:
        p.terminate(); p.wait()

def test_instance_live_true_on_cmdline_match(monkeypatch):
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 1,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token",
                            lambda pid, tok: tok == "omni-acc0")
        assert runtime.instance_live(rec) is True   # cheap path, no socket
    finally:
        p.terminate(); p.wait()

def test_instance_live_true_on_qmp_match_when_no_cmdline(monkeypatch):
    p = subprocess.Popen(["sleep", "30"])
    try:
        rec = {"name": "acc0", "pid": p.pid, "qmp_port": 5,
               "identity": "omni-acc0"}
        monkeypatch.setattr(runtime, "_cmdline_has_token", lambda pid, tok: False)
        monkeypatch.setattr(runtime, "_qmp_name",
                            lambda port, timeout=0.25: "omni-acc0")
        assert runtime.instance_live(rec) is True
    finally:
        p.terminate(); p.wait()

def test_instance_live_false_when_pid_dead(monkeypatch):
    rec = {"name": "acc0", "pid": 999999, "qmp_port": 1, "identity": "omni-acc0"}
    monkeypatch.setattr(runtime, "pid_alive", lambda pid: False)
    assert runtime.instance_live(rec) is False

def test_instance_live_legacy_record_falls_back_to_pid(monkeypatch):
    # No identity field (pre-upgrade run.json) -> trust pid_alive alone.
    rec = {"name": "acc0", "pid": 4242}
    monkeypatch.setattr(runtime, "pid_alive", lambda pid: True)
    assert runtime.instance_live(rec) is True

def test_allocate_ports_skips_a_port_that_answers(monkeypatch):
    # No run.json claims anything, but a live listener sits on index 0's qmp
    # port. allocate_ports must NOT hand out index 0.
    srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
    live_port = srv.getsockname()[1]
    cfg = {"qemu": {"adb_port_start": 6000, "qmp_port_start": live_port,
                    "vnc_port_start": 18001}}
    monkeypatch.setattr(runtime, "_claimed_port_indices", lambda: set())
    monkeypatch.setattr(runtime, "vnc_start", lambda c: 18001)
    try:
        adb_port, qmp_port, vnc_port = runtime.allocate_ports(cfg)
        assert qmp_port != live_port          # index 0 was skipped
    finally:
        srv.close()

def test_reconcile_gcs_dead_and_silent(tmp_path, monkeypatch):
    root = tmp_path / "runtime"; (root / "acc0").mkdir(parents=True)
    (root / "acc0" / "run.json").write_text(json.dumps(
        {"pid": 999999, "identity": "omni-acc0", "qmp_port": 1,
         "adb_port": 2}))
    monkeypatch.setattr(runtime.config, "data_dir", lambda: tmp_path)
    monkeypatch.setattr(runtime, "instance_live", lambda rec: False)
    monkeypatch.setattr(runtime, "_port_answers", lambda port, timeout=0.25: False)
    result = runtime.reconcile_runtime()
    assert "acc0" in result["gc"]
    assert not (root / "acc0").exists()

def test_reconcile_keeps_live_instance(tmp_path, monkeypatch):
    root = tmp_path / "runtime"; (root / "acc0").mkdir(parents=True)
    (root / "acc0" / "run.json").write_text(json.dumps(
        {"pid": 4242, "identity": "omni-acc0", "qmp_port": 1, "adb_port": 2}))
    monkeypatch.setattr(runtime.config, "data_dir", lambda: tmp_path)
    monkeypatch.setattr(runtime, "instance_live", lambda rec: True)
    result = runtime.reconcile_runtime()
    assert result["gc"] == []
    assert (root / "acc0").exists()
