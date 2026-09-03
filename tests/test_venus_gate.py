"""OMNI_VENUS=1 gate: venus/Vulkan suboptions on the virtio-gpu-gl-pci device.

A/B testing hatch only -- never on by default. See HANDOFF-VENUS.md.
`venus_enabled()` follows force_video_mode()'s truthiness style (env first,
then config `qemu.venus`), and `gpu_extra_opts()` prepends VENUS_GPU_OPTS
ahead of any explicit OMNI_GPU_OPTS/`qemu.gpu_opts` when the gate is on.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import qemu_proc  # noqa: E402


def _clear(monkeypatch):
    monkeypatch.delenv("OMNI_VENUS", raising=False)
    monkeypatch.delenv("OMNI_GPU_OPTS", raising=False)


def test_unset_is_unchanged(monkeypatch):
    _clear(monkeypatch)
    assert qemu_proc.venus_enabled() is False
    assert qemu_proc.gpu_extra_opts() == ""
    assert qemu_proc.gl_device_arg((800, 1280)) == "virtio-gpu-gl-pci,xres=800,yres=1280"


def test_omni_venus_1_appends_venus_opts_to_device_arg(monkeypatch):
    _clear(monkeypatch)
    monkeypatch.setenv("OMNI_VENUS", "1")
    assert qemu_proc.venus_enabled() is True
    assert qemu_proc.gpu_extra_opts() == qemu_proc.VENUS_GPU_OPTS
    arg = qemu_proc.gl_device_arg((800, 1280))
    assert arg.endswith("venus=on,blob=true,hostmem=1G")


def test_omni_venus_0_is_unchanged(monkeypatch):
    _clear(monkeypatch)
    monkeypatch.setenv("OMNI_VENUS", "0")
    assert qemu_proc.venus_enabled() is False
    assert qemu_proc.gpu_extra_opts() == ""


def test_config_qemu_venus_true_enables_it(monkeypatch):
    _clear(monkeypatch)
    cfg = {"qemu": {"venus": True}}
    assert qemu_proc.venus_enabled(cfg) is True
    assert qemu_proc.gpu_extra_opts(cfg) == qemu_proc.VENUS_GPU_OPTS


def test_combined_with_omni_gpu_opts_both_present(monkeypatch):
    _clear(monkeypatch)
    monkeypatch.setenv("OMNI_VENUS", "1")
    monkeypatch.setenv("OMNI_GPU_OPTS", "foo=bar")
    got = qemu_proc.gpu_extra_opts()
    assert got == "venus=on,blob=true,hostmem=1G,foo=bar"
    assert not got.startswith(",")
    assert not got.endswith(",")
