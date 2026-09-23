"""Shared helpers for the Linux row files. Not a row file itself (the leading `_`)."""

import os

HOME = os.path.expanduser("~")


def with_env(env, command):
    """`command` with extra environment, spelled with `env(1)` so a row stays a plain argv."""
    return ["env"] + [f"{k}={v}" for k, v in env.items()] + command
