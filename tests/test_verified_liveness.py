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
    qemu_proc.spawn_qemu(acct, {"qemu": {}}, dev=False)
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
