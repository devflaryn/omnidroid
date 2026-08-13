# omnidroid/output.py
"""CLI output primitives: the single JSON payload channel, --json mode, and
the contract-shaped fatal-error helper. Cross-cutting, zero omnidroid deps."""
import sys
import json


def write_safely(stream, text):
    """Write to a stream that may be dead, and never raise.

    The engine usually runs INSIDE the GUI: its stdout/stderr are pipes owned
    by the app. When the app stops reading them -- its watchdog timer fires
    and kills the read end, the window closes, the Popen object is collected
    -- the next write from the engine fails. On Windows that surfaces as
    OSError(EINVAL, "Invalid argument"), which in a frozen windowed build
    means a PyInstaller "Unhandled exception in script" dialog and a dead
    launch.

    A PROGRESS LINE MUST NEVER KILL A BOOT. Losing the message is the correct
    trade: the instance is fine, only the narration is gone. Observed exactly
    this: wait_for_boot printed its 15-second progress line after the GUI had
    given up, and the whole start died with Errno 22.

    A frozen windowed process can also have `None` for stdio when nothing is
    piped at all, which is why the None check comes first.
    """
    if stream is None:
        return False
    try:
        stream.write(text)
        stream.flush()
        return True
    except (OSError, ValueError, AttributeError):
        # OSError: dead pipe / invalid handle. ValueError: closed file.
        # AttributeError: a stub stream without write/flush.
        return False


def emit_json(obj):
    """The one JSON payload a --json command prints on stdout."""
    write_safely(sys.stdout, json.dumps(obj) + "\n")


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
        try:
            orig(*a, **k)
        except (OSError, ValueError, AttributeError):
            # The GUI owns this pipe and may have stopped reading it; see
            # write_safely. Never let narration kill the command.
            pass
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
    write_safely(sys.stderr, f"error: {msg}\n")
    sys.exit(exit_code)


def redact_token(tok):
    """Never print a token. Enough tail to tell two tokens apart in a log, never
    enough to use one."""
    if not tok:
        return None
    return f"<{len(tok)} chars, ...{tok[-6:]}>"
