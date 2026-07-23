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
