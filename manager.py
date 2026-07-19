#!/usr/bin/env python3
"""Thin shim so `python manager.py <cmd>` keeps working without install."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from omnidroid.cli import main

if __name__ == "__main__":
    main()
