import importlib


def test_omnidroid_package_imports():
    mod = importlib.import_module("omnidroid")
    assert isinstance(mod.__version__, str)
    assert mod.__version__
