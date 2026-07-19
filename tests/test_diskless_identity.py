"""Task 1 of plan A2.2: `load_account(name)` becomes a store+runtime handle
builder that reads NO per-account folder.

Identity (base mode) comes from the central cookie store (omnidroid/accounts.py);
live ports (and, if the instance is actually running, the exact base TAG) come
from runtime/<username>/run.json. This is the linchpin that lets the ~14
running-instance commands (start/stop/status/screenshot/...) keep working once
accounts/<name>/ stops existing.

    python3 -m pytest tests/test_diskless_identity.py -q
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import pytest  # noqa: E402

from omnidroid import accounts, engine  # noqa: E402


def _write_run(tmp, name, **fields):
    d = tmp / "runtime" / name
    d.mkdir(parents=True, exist_ok=True)
    (d / "run.json").write_text(json.dumps(fields))


def test_store_account_with_running_instance_yields_handle_from_run_json(
        tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "u", "cookie", 1)
    accounts.set_fields(tmp_path, "u", base="dev")
    _write_run(tmp_path, "u", pid=os.getpid(), adb_port=16005,
               qmp_port=17005, vnc_port=18005, base="dev")

    handle = engine.load_account("u")

    assert handle["name"] == "u"
    assert handle["base"] == "dev"          # taken straight from run.json
    assert handle["adb_port"] == 16005
    assert handle["qmp_port"] == 17005
    assert handle["vnc_port"] == 18005
    assert handle["ephemeral"] is True
    assert handle["first_boot_done"] is True
    assert handle["game_package"] == engine.ROBLOX_PACKAGE
    assert handle["dev"] is True


def test_store_account_prod_mode_no_runtime_yields_identity_only_handle(
        tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "p", "cookie", 2)
    accounts.set_fields(tmp_path, "p", base="prod")

    handle = engine.load_account("p")

    assert handle["name"] == "p"
    assert handle["dev"] is False
    assert handle["base"]                     # truthy: a resolved cfg base tag
    assert engine.base_is_dev(
        engine.read_config()["bases"].get(handle["base"], {})) is False
    assert handle.get("adb_port") is None
    assert handle["ephemeral"] is True
    assert handle["first_boot_done"] is True
    assert handle["game_package"] == engine.ROBLOX_PACKAGE


def test_unknown_account_exits(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    with pytest.raises(SystemExit):
        engine.load_account("ghost-of-nobody")


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
