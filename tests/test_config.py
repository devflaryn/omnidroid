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
