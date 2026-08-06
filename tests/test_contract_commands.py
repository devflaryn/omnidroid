#!/usr/bin/env python3
"""`version --json` must advertise the commands this engine actually accepts.

    python3 tests/test_contract_commands.py

This is the client-facing contract: omni-executor (and omni-agent) read
`commands` to decide what the engine can do. It used to be a hand-maintained
literal, and it had drifted from the code it describes:

  * it advertised `create`, which no longer exists — a client following the
    contract would invoke a dead command;
  * it omitted `setup`, `login` and `view` — the three calls omni-executor
    actually makes, so a client checking the list would refuse to make them.

Neither failure is visible from inside the engine; both surface as a broken
GUI. So the list is now DERIVED from the parser, and these tests exist to
keep it that way rather than to re-check a literal.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _parser_commands():
    parser = omni.build_parser()
    for action in parser._actions:
        if getattr(action, "dest", None) == "cmd" and action.choices:
            return sorted(action.choices)
    raise AssertionError("no subcommand action on the parser")


class RegisteredCommands(unittest.TestCase):
    def test_matches_the_parser_exactly(self):
        self.assertEqual(omni.registered_commands(), _parser_commands())

    def test_is_not_empty(self):
        self.assertTrue(omni.registered_commands())

    def test_every_advertised_command_actually_parses(self):
        """Advertising a command a client cannot invoke is the exact failure
        this replaced: `create` was advertised long after it was removed."""
        parser = omni.build_parser()
        for cmd in omni.registered_commands():
            with self.subTest(cmd=cmd):
                self.assertIn(cmd, parser._subparsers._group_actions[0].choices)

    def test_removed_commands_are_not_advertised(self):
        self.assertNotIn("create", omni.registered_commands())


class ExecutorContract(unittest.TestCase):
    """The calls omni-executor makes must all be advertised AND accept the
    exact argv it builds. This is the cross-repo guard: renaming a command or
    dropping a flag here breaks the GUI, and nothing else in this suite would
    notice."""

    # Mirrors omni-executor main.py's run_engine([...]) call sites.
    CALLS = (
        ["version", "--json"],
        ["doctor", "--json"],
        ["bases", "--json"],
        ["use-base", "arm"],
        ["setup"],
        ["list", "--json"],
        ["login"],
        ["login", "--token-file", "/tmp/t", "--json"],
        ["start", "acct", "--json"],
        ["start", "acct", "--json", "--mode", "farming"],
        ["start", "acct", "--json", "--mode", "farming", "--place", "123"],
        ["stop", "acct", "--json"],
        ["remove", "acct", "--json"],
        ["view", "acct", "--start"],
    )

    def test_every_executor_command_is_advertised(self):
        advertised = set(omni.registered_commands())
        for argv in self.CALLS:
            with self.subTest(cmd=argv[0]):
                self.assertIn(argv[0], advertised)

    def test_every_executor_argv_parses(self):
        import contextlib
        import io
        parser = omni.build_parser()
        for argv in self.CALLS:
            with self.subTest(argv=" ".join(argv)):
                err = io.StringIO()
                try:
                    with contextlib.redirect_stderr(err):
                        parser.parse_args(argv)
                except SystemExit:
                    self.fail(f"engine rejected `{' '.join(argv)}`: "
                              f"{err.getvalue().strip()[-200:]}")

    def test_modes_the_executor_offers_are_real(self):
        """The GUI's mode dropdown is populated from version.modes and passed
        straight back as --mode, so every advertised mode must be accepted."""
        parser = omni.build_parser()
        for mode in omni.MODES:
            with self.subTest(mode=mode):
                parser.parse_args(["start", "acct", "--json", "--mode", mode])


if __name__ == "__main__":
    unittest.main()
