"""Console entry point. All command logic lives in omnidroid.engine."""
from omnidroid.engine import main


def main_entry():
    main()


# pyproject's console_scripts points at `omnidroid.cli:main`.
__all__ = ["main"]
