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


def _build_cfg(images_dir):
    return {
        "images_dir": str(images_dir),
        "qemu": {"adb_port_start": 16001, "qmp_port_start": 17001,
                 "vnc_port_start": 18001},
        "bases": {
            "arm": {"type": engine.BASE_TYPE_ARM, "base_disk": "b.qcow2",
                    "system": "s.qcow2", "data": "d.qcow2",
                    "efivars": "base_arm_efivars.fd"},
        },
        "current_base": "arm",
    }


def test_build_acct_allocates_ports_and_writes_no_account_folder(
        tmp_path, monkeypatch):
    """`build_acct` is the launch handle: it allocates ports up front (unlike
    load_account, which only sees ports for an ALREADY running instance) and
    stages a fresh efivars into runtime/<name>/ -- but it must never create
    accounts/<name>/, since ephemeral instances boot the shared base
    templates directly and have nothing per-account to persist."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    images = tmp_path / "images"
    images.mkdir()
    (images / "base_arm_efivars.fd").write_bytes(b"EFI-TEMPLATE")
    cfg = _build_cfg(images)
    monkeypatch.setattr(engine, "ensure_qemu", lambda: None)

    handle = engine.build_acct("newacct", cfg)

    assert handle["name"] == "newacct"
    assert handle["base"] == "arm"
    assert handle["ephemeral"] is True
    assert handle["dev"] is False
    assert handle["first_boot_done"] is True
    assert handle["game_package"] == engine.ROBLOX_PACKAGE
    assert handle["adb_port"] == 16001
    assert handle["qmp_port"] == 17001
    assert handle["vnc_port"] == 18001

    assert (tmp_path / "runtime" / "newacct" / "efivars.fd").read_bytes() \
        == b"EFI-TEMPLATE"
    assert not (tmp_path / "accounts" / "newacct").exists()


def test_build_acct_bad_name_fails(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    cfg = _build_cfg(tmp_path / "images")
    monkeypatch.setattr(engine, "ensure_qemu", lambda: None)
    with pytest.raises(SystemExit):
        engine.build_acct("bad name!", cfg)


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
