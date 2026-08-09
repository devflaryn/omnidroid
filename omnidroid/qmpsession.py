"""A persistent QMP session, for handshakes that span several commands.

qemu_proc.qmp() opens one connection per command, which is right for the
fire-and-forget calls the engine already makes (balloon, quit, query-balloon).
It cannot express a migration, where capabilities must be negotiated and THEN
`migrate`/`migrate-incoming` issued on the same connection -- get that wrong
and the destination dies with:

    Capability mapped-ram is off, but received capability is on
"""
import json
import socket
import time

MIGRATION_CAPS = ("mapped-ram", "multifd")


class QmpSession:
    """One long-lived QMP connection. Use as a context manager."""

    def __init__(self, port, connect_timeout=60.0, timeout=15.0):
        deadline = time.monotonic() + connect_timeout
        last = None
        while True:
            try:
                self._sock = socket.create_connection(("127.0.0.1", port),
                                                        timeout=timeout)
                break
            except OSError as e:
                last = e
                if time.monotonic() >= deadline:
                    raise OSError(
                        f"QMP on 127.0.0.1:{port} never accepted a connection "
                        f"within {connect_timeout}s: {last}") from last
                time.sleep(0.25)
        self._sock.settimeout(timeout)
        self._f = None
        try:
            self._f = self._sock.makefile("rw", encoding="utf-8", newline="\n")
            greeting = self._f.readline()             # greeting
            if not greeting:
                raise OSError(
                    f"QMP on 127.0.0.1:{port} closed the connection before "
                    f"sending its greeting")
            caps = self.cmd("qmp_capabilities")
            if "error" in caps:
                raise OSError(
                    f"QMP on 127.0.0.1:{port} rejected qmp_capabilities: "
                    f"{caps['error']}")
        except Exception:
            # Don't leak the fd: every retried cold-boot fallback would
            # otherwise leak another one.
            self.close()
            raise

    def cmd(self, execute, arguments=None):
        """Send one command, return its parsed reply (return OR error).

        Asynchronous events are skipped: only a reply carries `return`/`error`.
        A write/flush failure (QEMU already dead) degrades to the same error
        dict shape as a closed connection, rather than raising -- callers
        fall back to a cold boot on an error dict, not on an exception.
        """
        msg = {"execute": execute}
        if arguments:
            msg["arguments"] = arguments
        try:
            self._f.write(json.dumps(msg) + "\n")
            self._f.flush()
        except (OSError, ValueError) as e:
            return {"error": {"desc": f"QMP connection closed: {e}"}}
        while True:
            line = self._f.readline()
            if not line:
                return {"error": {"desc": "QMP connection closed"}}
            reply = json.loads(line)
            if "return" in reply or "error" in reply:
                return reply

    def set_migration_caps(self, channels=4):
        """Enable the capabilities the warm-restore stream is written with.

        Must be called on BOTH ends, and on the destination BEFORE
        migrate-incoming. direct-io is deliberately not set: the Homebrew QEMU
        build reports "No build-time support for direct-io" and the mechanism
        works fine without it.
        """
        self.cmd("migrate-set-capabilities",
                 {"capabilities": [{"capability": c, "state": True}
                                    for c in MIGRATION_CAPS]})
        self.cmd("migrate-set-parameters", {"multifd-channels": channels})

    def wait_migrate(self, timeout=600.0, sleep=0.25):
        """Poll query-migrate until terminal. Returns the status string, or
        'timeout' -- never hangs, so a stuck migration degrades to a cold boot.

        An `{"error": ...}` reply is ALSO terminal, treated as 'failed'. This
        is what a dead QMP connection looks like: cmd() degrades a closed
        socket or a write failure to that same error-dict shape rather than
        raising (see cmd()'s own docstring), so without this check a QEMU
        that exited mid-migration would report a status of None forever and
        this loop would spin in a tight 0.25s poll for the full `timeout` --
        on the bake path that is 10 minutes with the guest already stopped.
        """
        deadline = time.monotonic() + timeout
        while True:
            reply = self.cmd("query-migrate")
            if "error" in reply:
                return "failed"
            status = (reply.get("return", {}) or {}).get("status")
            if status in ("completed", "failed", "cancelled"):
                return status
            if time.monotonic() >= deadline:
                return "timeout"
            time.sleep(sleep)

    def close(self):
        try:
            self._f.close()
        except Exception:      # noqa: BLE001
            pass
        try:
            self._sock.close()
        except Exception:      # noqa: BLE001
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False
