import os
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import config  # noqa: E402


def test_repo_root_is_project_dir():
    assert (config.REPO / "omnidroid" / "engine.py").exists()


def test_platform_flags_are_mutually_consistent():
    assert sum([config.IS_WINDOWS, config.IS_LINUX, config.IS_MACOS]) == 1


def test_qemu_system_name_matches_host():
    name = config.qemu_system_name()
    assert name in ("qemu-system-aarch64", "qemu-system-x86_64")


def test_resolve_images_dir_expands_and_absolutizes():
    got = config.resolve_images_dir({"images_dir": "~/OmniImages"})
    assert Path(got).is_absolute()
    assert "~" not in got


def test_data_dir_defaults_to_repo(monkeypatch):
    monkeypatch.delenv("OMNI_DATA_DIR", raising=False)
    assert config.data_dir() == config.REPO


def test_data_dir_env_override(tmp_path, monkeypatch):
    target = tmp_path / "omnidata"
    monkeypatch.setenv("OMNI_DATA_DIR", str(target))
    got = config.data_dir()
    assert got == target
    assert got.exists()  # created on demand


def test_images_dir_env_override(tmp_path, monkeypatch):
    monkeypatch.setenv("OMNI_IMAGES_DIR", str(tmp_path / "imgs"))
    got = config.images_dir({"images_dir": "~/OmniImages"})
    assert Path(got) == (tmp_path / "imgs")


def test_images_dir_no_env_uses_config(tmp_path, monkeypatch):
    monkeypatch.delenv("OMNI_IMAGES_DIR", raising=False)
    got = config.images_dir({"images_dir": str(tmp_path / "cfgimgs")})
    assert Path(got) == (tmp_path / "cfgimgs")


def test_engine_imports_the_override_aware_images_dir():
    """engine.py must call config.images_dir() (the OMNI_IMAGES_DIR-aware
    wrapper), not just config.resolve_images_dir() (which ignores the env
    override) -- this was the Fix-1 bug: images_dir() existed and was unit
    tested but engine never called it, so the override was dead end-to-end.
    Identity check confirms engine's `images_dir` name is truly bound to
    config's override-aware function (not a shadowing local of the same
    name)."""
    from omnidroid import engine
    assert engine.images_dir is config.images_dir


def test_install_readiness_honors_images_dir_override(tmp_path, monkeypatch):
    """End-to-end: OMNI_IMAGES_DIR must change the path an actual ENGINE
    code path resolves and reports, not just the config.images_dir()
    function in isolation. install_readiness() is the doctor/setup
    readiness report; it calls autoregister_bases() (which itself calls
    images_dir()) and separately calls images_dir() again to build the
    report -- both are Fix-1 call sites. tmp_path is empty on purpose: no
    base files live there, so autoregister_bases() finds nothing to
    register and leaves configs/paths.json untouched (no side effects on
    the real repo config from this test)."""
    from omnidroid import engine
    monkeypatch.setenv("OMNI_IMAGES_DIR", str(tmp_path))
    rep = engine.install_readiness()
    assert rep["images_dir"] == str(tmp_path)
    assert rep["images_dir_exists"] is True
