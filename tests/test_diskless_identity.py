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
from types import SimpleNamespace

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
    accounts.set_fields(tmp_path, "u", base="prod")
    # A DEBUG boot records debug=True in run.json (a per-boot property).
    _write_run(tmp_path, "u", pid=os.getpid(), adb_port=16005,
               qmp_port=17005, vnc_port=18005, base="arm", debug=True)

    handle = engine.load_account("u")

    assert handle["name"] == "u"
    assert handle["base"] == "arm"          # taken straight from run.json
    assert handle["adb_port"] == 16005
    assert handle["qmp_port"] == 17005
    assert handle["vnc_port"] == 18005
    assert handle["ephemeral"] is True
    assert handle["first_boot_done"] is True
    assert handle["game_package"] == engine.ROBLOX_PACKAGE
    assert handle["debug"] is True          # per-boot, from run.json


def test_store_account_prod_mode_no_runtime_yields_identity_only_handle(
        tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "p", "cookie", 2)
    accounts.set_fields(tmp_path, "p", base="prod")

    handle = engine.load_account("p")

    assert handle["name"] == "p"
    assert handle["debug"] is False           # not running -> never debug
    assert handle["base"]                     # truthy: a resolved cfg base tag
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
    monkeypatch.setattr("omnidroid.runtime._port_answers", lambda port, timeout=0.25: False)

    handle = engine.build_acct("newacct", cfg)

    assert handle["name"] == "newacct"
    assert handle["base"] == "arm"
    assert handle["ephemeral"] is True
    assert handle["debug"] is False
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


def test_all_accounts_outer_joins_running_instances_not_in_store(
        tmp_path, monkeypatch):
    """all_accounts() must be a FULL OUTER JOIN of the cookie store and live
    runtime state: a running instance whose name is NOT in the store (e.g. a
    temp build/bench instance) must still be surfaced, or the
    running-instance safety guards in cmd_bake_game / cmd_brand_base
    --in-place / update_kiosk_arm silently go blind to it."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "insstore", "cookie", 1)
    _write_run(tmp_path, "insstore", pid=os.getpid(), adb_port=16001,
               qmp_port=17001, vnc_port=18001, base="prod")
    _write_run(tmp_path, "tempbench", pid=os.getpid(), adb_port=16002,
               qmp_port=17002, vnc_port=18002, base="prod")

    names = {a["name"] for a in engine.all_accounts()}

    assert "insstore" in names
    assert "tempbench" in names


# ---------- Task 5: remove -> store delete + wipe runtime; stop -> wipe ----

def test_cmd_remove_deletes_store_entry_and_wipes_runtime(tmp_path, monkeypatch):
    """`remove` in the diskless model has no per-account folder to delete: the
    account IS the store entry (omnidroid/accounts.py) plus whatever it left
    in runtime/<name>/. Seed both (a dead pid, so the instance already reads
    as stopped) and confirm cmd_remove clears the store record AND wipes the
    runtime dir."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "gone", "cookie", 1)
    accounts.set_fields(tmp_path, "gone", base="prod")
    _write_run(tmp_path, "gone", pid=999999999, adb_port=16001,
               qmp_port=17001, vnc_port=18001, base="prod")
    assert accounts.get_account(tmp_path, "gone") is not None
    assert (tmp_path / "runtime" / "gone").exists()

    args = SimpleNamespace(name="gone", timeout=5, json=True)
    engine.cmd_remove(args)

    assert accounts.get_account(tmp_path, "gone") is None
    assert not (tmp_path / "runtime" / "gone").exists()


def test_cmd_remove_unknown_account_exits(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    args = SimpleNamespace(name="never-existed", timeout=5, json=True)
    with pytest.raises(SystemExit):
        engine.cmd_remove(args)


def test_cmd_stop_wipes_runtime_after_successful_shutdown(tmp_path, monkeypatch):
    """The core diskless 'wipe on stop' promise: after a shutdown that did
    NOT have to fall back to a kill (method != 'kill-failed'), runtime/<name>/
    (efivars, run.json, qemu.log, autocap frames) must be gone -- that dir is
    the entire per-instance footprint an ephemeral account leaves."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "s", "cookie", 1)
    accounts.set_fields(tmp_path, "s", base="prod")
    _write_run(tmp_path, "s", pid=os.getpid(), adb_port=16001,
               qmp_port=17001, vnc_port=18001, base="prod")
    monkeypatch.setattr(engine, "_shutdown",
                        lambda acct, label, timeout=90: "powerdown")

    args = SimpleNamespace(name="s", timeout=5, json=True)
    engine.cmd_stop(args)

    assert not (tmp_path / "runtime" / "s").exists()


def test_cmd_stop_does_not_wipe_runtime_on_kill_failed(tmp_path, monkeypatch):
    """A failed kill means the instance may still be alive -- wiping run.json
    here would orphan a live QEMU process (running_pid would stop seeing it),
    so the runtime dir must survive a kill-failed stop."""
    monkeypatch.setenv("OMNI_DATA_DIR", str(tmp_path))
    accounts.save_account(tmp_path, "stuck", "cookie", 1)
    accounts.set_fields(tmp_path, "stuck", base="prod")
    _write_run(tmp_path, "stuck", pid=os.getpid(), adb_port=16003,
               qmp_port=17003, vnc_port=18003, base="prod")
    monkeypatch.setattr(engine, "_shutdown",
                        lambda acct, label, timeout=90: "kill-failed")

    args = SimpleNamespace(name="stuck", timeout=5, json=True)
    with pytest.raises(SystemExit):
        engine.cmd_stop(args)

    assert (tmp_path / "runtime" / "stuck").exists()


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
