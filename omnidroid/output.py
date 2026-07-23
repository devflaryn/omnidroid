# omnidroid/output.py
"""CLI output primitives: the single JSON payload channel, --json mode, and
the contract-shaped fatal-error helper. Cross-cutting, zero omnidroid deps."""
import sys
import json


def emit_json(obj):
    """The one JSON payload a --json command prints on stdout."""
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


# Set True by enable_json_mode(); read by fail() to shape typed errors.
_JSON_MODE = False


def enable_json_mode():
    """--json: stdout must carry EXACTLY the JSON payload. Redirect every
    informational print() (progress, warnings) to stderr so a GUI can
    parse stdout blindly. emit_json writes to sys.stdout directly and is
    unaffected."""
    global _JSON_MODE
    _JSON_MODE = True
    import builtins
    orig = builtins.print

    def _to_stderr(*a, **k):
        k.setdefault("file", sys.stderr)
        orig(*a, **k)
    builtins.print = _to_stderr


def fail(code, message=None, exit_code=1):
    """Contract-shaped fatal error (omnidroid-api.md v1 §8). In --json mode
    emit {"ok":false,"error":<code>,"message":<msg>} on stdout; always write a
    human line to stderr; exit nonzero. Use for the TYPED errors the contract
    names (arch_boundary, abi_not_translated, install_failed, no_base, ...);
    legacy sys.exit(str) sites are left untouched to keep [CURRENT] behavior."""
    msg = message or code
    if _JSON_MODE:
        emit_json({"ok": False, "error": code, "message": msg})
    sys.stderr.write(f"error: {msg}\n")
    sys.exit(exit_code)


def redact_token(tok):
    """Never print a token. Enough tail to tell two tokens apart in a log, never
    enough to use one."""
    if not tok:
        return None
    return f"<{len(tok)} chars, ...{tok[-6:]}>"
