"""Hyphenated entry point; the module is tools/build_qemu.py."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from tools.build_qemu import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
