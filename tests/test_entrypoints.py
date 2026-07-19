import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _run(args):
    return subprocess.run([sys.executable, *args], cwd=ROOT,
                          capture_output=True, text=True, timeout=60)


def test_module_route():
    r = _run(["-m", "omnidroid", "version"])
    assert r.returncode == 0, r.stderr


def test_manager_shim_route():
    r = _run(["manager.py", "version"])
    assert r.returncode == 0, r.stderr


def test_cli_main_is_callable():
    sys.path.insert(0, ROOT)
    from omnidroid import cli
    assert callable(cli.main)
