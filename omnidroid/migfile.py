"""Move a QEMU migration stream between a running VM and a file on disk.

This exists because **QEMU cannot migrate to a file on Windows**, and the warm
boot cache is built entirely on migrating to a file.

MEASURED 2026-08-15, QEMU 11.0.50 (the exact build the product ships), a
throwaway 256 MB guest, every combination tried:

    caps                       result
    -------------------------  ------------------------------------------
    mapped-ram + multifd       QEMU DIED mid-command (QMP reset)
    mapped-ram                 status=failed, 0 bytes written
    (none) - plain file:       status=failed, 0 bytes written

and the reason, straight out of `query-migrate`:

    "error-desc": "Failed to set FD nonblocking: Input/output error"

That is not a build option or a capability negotiation: Windows only supports
non-blocking I/O on SOCKETS, never on file handles, so the moment QEMU wraps
the destination file in a QIOChannel and asks for non-blocking mode it fails.
No combination of migration capabilities can get around it, because the failure
is below all of them.

Sockets, on the other hand, work fine -- migrating the same guest to
`tcp:127.0.0.1:<port>` completed in 0.2 s. So on Windows this module puts a
socket in the middle and does the file I/O itself:

    bake     host listens  ->  QEMU connects out  ->  host writes the file
    restore  QEMU listens  ->  host connects in   ->  host feeds the file

Everywhere else `file:` is used directly, with mapped-ram + multifd, because
there it is both supported and better: mapped-ram writes each page at a fixed
offset, so the file is SPARSE (it costs the guest's resident set, not its -m
size) and the restore reads it in parallel.

The transport used is recorded in the entry's meta.json, because the two
formats are not interchangeable -- a mapped-ram file restored without the
capability set is rejected, which would look exactly like a corrupt entry.
"""
import socket
import threading
import time
from pathlib import Path

from omnidroid.config import IS_WINDOWS

# The relay's socket buffer. 1 MiB: big enough that a multi-GB stream is not
# dominated by syscall count, small enough not to matter next to the guest.
CHUNK = 1 << 20

# How long the host waits for QEMU to connect out (bake) or to start listening
# (restore). Both are local and immediate in practice; this only bounds the
# failure case, and a failure here degrades to a cold boot.
CONNECT_TIMEOUT = 60.0

TRANSPORT_FILE = "file"
TRANSPORT_TCP = "tcp"

# Accelerators that cannot save or restore VM state AT ALL, so nothing built on
# migration -- the whole warm-boot cache -- can work under them.
#
# WHPX is one, and it is the accelerator the Windows product uses. QEMU
# registers a migration blocker for it at CPU realize time and the message is
# verbatim from target/i386/whpx/whpx-all.c:
#
#     warm bake failed (migration State blocked due to non-migratable CPUID
#     feature support,dirty memory tracking support, and XSAVE/XRSTOR support)
#
# MEASURED 2026-08-15 against a real booted instance, after the transport
# question below had been solved -- i.e. the stream had somewhere to go and
# QEMU still refused. Windows Hypervisor Platform exposes no way to read back
# guest CPUID state, no dirty-page log and no XSAVE area, so there is nothing
# for QEMU to serialise. No capability, transport or flag changes this; it
# needs a different hypervisor.
#
# Gating on it matters because a bake is not free: it STOPS the guest, stages
# two qcow2 overlays and drives a migration that is going to be refused, on
# every single launch, forever, for a cache that can never be populated.
NON_MIGRATABLE_ACCELS = ("whpx",)


def accel_supports_migration(accel):
    """Can a VM under this `-accel` string be frozen and restored?

    Takes the raw accel string (e.g. "whpx,kernel-irqchip=off") and looks only
    at the accelerator name, which is the part that decides.
    """
    name = str(accel or "").split(",")[0].strip().lower()
    return name not in NON_MIGRATABLE_ACCELS


def default_transport():
    """Which transport this HOST can use. See the module docstring."""
    return TRANSPORT_TCP if IS_WINDOWS else TRANSPORT_FILE


def transport_caps(transport):
    """Migration capabilities that go with a transport.

    mapped-ram is a FILE-ONLY capability -- it means "write each page at its
    own offset in the destination", which a stream socket cannot express. It
    also implies multifd here, since parallel channels are the whole reason to
    want fixed offsets. A TCP relay takes neither: one ordered stream, which is
    also what keeps the host side of the relay a single loop.
    """
    if transport == TRANSPORT_FILE:
        return ("mapped-ram", "multifd")
    return ()


def _pick_port(preferred=None):
    """A bindable loopback port: `preferred` if free, else an ephemeral one.

    Bind, don't connect, to decide: a closed loopback port on this Windows host
    is DROPPED rather than refused (see runtime._port_answers), so a
    connect-based probe hangs instead of answering.
    """
    for candidate in ([preferred] if preferred else []) + [0]:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", candidate))
            port = s.getsockname()[1]
            return port, s
        except OSError:
            s.close()
    raise OSError("no loopback port available for the migration relay")


class _Sink(threading.Thread):
    """Accept ONE connection and write everything it sends into `path`."""

    def __init__(self, server, path):
        super().__init__(daemon=True)
        self.server = server
        self.path = Path(path)
        self.bytes = 0
        self.error = None

    def run(self):
        conn = None
        try:
            self.server.settimeout(CONNECT_TIMEOUT)
            conn, _ = self.server.accept()
            conn.settimeout(CONNECT_TIMEOUT)
            with open(self.path, "wb") as fh:
                while True:
                    try:
                        chunk = conn.recv(CHUNK)
                    except ConnectionResetError:
                        # QEMU closes its end hard the moment the stream is
                        # finished, and Windows surfaces that as RST rather
                        # than a clean FIN. Observed on the very first test
                        # migration. It is only safe to treat as end-of-stream
                        # because the CALLER separately requires
                        # query-migrate to report `completed`, and because the
                        # entry is validated by restoring from it before
                        # anything trusts it.
                        break
                    if not chunk:
                        break
                    fh.write(chunk)
                    self.bytes += len(chunk)
                fh.flush()
        except Exception as e:      # noqa: BLE001 - a failed bake is not a failed launch
            self.error = e
        finally:
            for sock in (conn, self.server):
                try:
                    if sock is not None:
                        sock.close()
                except OSError:
                    pass


class _Source(threading.Thread):
    """Connect to a listening QEMU and feed it the whole of `path`."""

    def __init__(self, port, path):
        super().__init__(daemon=True)
        self.port = port
        self.path = Path(path)
        self.bytes = 0
        self.error = None

    def run(self):
        conn = None
        try:
            deadline = time.monotonic() + CONNECT_TIMEOUT
            while True:
                try:
                    conn = socket.create_connection(("127.0.0.1", self.port),
                                                    timeout=5)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            conn.settimeout(CONNECT_TIMEOUT)
            with open(self.path, "rb") as fh:
                while True:
                    chunk = fh.read(CHUNK)
                    if not chunk:
                        break
                    conn.sendall(chunk)
                    self.bytes += len(chunk)
            # Half-close so QEMU sees end-of-stream rather than waiting for a
            # section header that will never come.
            try:
                conn.shutdown(socket.SHUT_WR)
            except OSError:
                pass
        except Exception as e:      # noqa: BLE001 - degrade to a cold boot
            self.error = e
        finally:
            try:
                if conn is not None:
                    conn.close()
            except OSError:
                pass


def save_state(session, path, transport=None, preferred_port=None):
    """Migrate the (already stopped) guest into `path`.

    `session` is a QmpSession with its capabilities already set for this
    transport. Returns (ok, detail); never raises.
    """
    transport = transport or default_transport()
    if transport == TRANSPORT_FILE:
        reply = session.cmd("migrate", {"uri": f"file:{path}"})
        if "error" in reply:
            return False, reply["error"].get("desc", "migrate rejected")
        status = session.wait_migrate()
        return status == "completed", status

    try:
        port, server = _pick_port(preferred_port)
    except OSError as e:
        return False, str(e)
    server.listen(1)
    sink = _Sink(server, path)
    sink.start()
    reply = session.cmd("migrate", {"uri": f"tcp:127.0.0.1:{port}"})
    if "error" in reply:
        server.close()
        sink.join(timeout=5)
        return False, reply["error"].get("desc", "migrate rejected")
    status = session.wait_migrate()
    sink.join(timeout=CONNECT_TIMEOUT)
    if status != "completed":
        return False, status
    if sink.error is not None:
        return False, f"relay failed: {sink.error!r}"
    if not sink.bytes:
        return False, "relay wrote nothing"
    short = _shortfall(session, sink.bytes)
    if short:
        return False, short
    return True, f"{sink.bytes} bytes over the tcp relay"


def _shortfall(session, written):
    """A message if fewer bytes reached the file than QEMU says it sent.

    The relay treats a connection RESET as end-of-stream, because that is how
    Windows ends a finished migration -- but a reset MID-stream looks exactly
    the same, and would write a truncated state file that passes every
    existence check and then fails the restore, weeks later, on a machine that
    is not this one. A unit test firing a genuine RST at the relay caught it
    writing 1.6 MB of a 2.9 MB payload and calling it a success.

    `query-migrate`'s `ram.transferred` is the cross-check, and it is one-sided
    on purpose: the file also carries device state and section headers, so it
    is always at least as large as the RAM figure. Fewer bytes than that means
    truncation; more means nothing. A QEMU that does not report the field is
    not treated as a failure -- an absent number cannot prove anything either
    way, and the bake is validated by an immediate restore regardless.
    """
    try:
        reply = session.cmd("query-migrate")
        transferred = int(((reply.get("return") or {}).get("ram") or {})
                          .get("transferred", 0))
    except Exception:      # noqa: BLE001 - an unreadable counter proves nothing
        return None
    if transferred and written < transferred:
        return (f"truncated: the relay wrote {written} bytes but QEMU "
                f"transferred {transferred} (the connection was reset "
                f"mid-stream)")
    return None


def load_state(session, path, transport=None, preferred_port=None):
    """Drive a deferred incoming migration from `path` into a spawned QEMU.

    The QEMU must already have been started with `-incoming defer` and have had
    its capabilities set for this transport. Returns (ok, detail).
    """
    transport = transport or default_transport()
    if transport == TRANSPORT_FILE:
        reply = session.cmd("migrate-incoming", {"uri": f"file:{path}"})
        if "error" in reply:
            return False, reply["error"].get("desc", "migrate-incoming rejected")
        status = session.wait_migrate()
        return status == "completed", status

    try:
        port, probe = _pick_port(preferred_port)
    except OSError as e:
        return False, str(e)
    # QEMU is the LISTENER on this side, so the probe socket has to be released
    # before it can bind the port. The window between close and QEMU's bind is
    # the only race here; a lost race fails the restore, which cold-boots.
    probe.close()
    reply = session.cmd("migrate-incoming", {"uri": f"tcp:127.0.0.1:{port}"})
    if "error" in reply:
        return False, reply["error"].get("desc", "migrate-incoming rejected")
    source = _Source(port, path)
    source.start()
    status = session.wait_migrate()
    source.join(timeout=CONNECT_TIMEOUT)
    if status != "completed":
        return False, status
    if source.error is not None:
        return False, f"relay failed: {source.error!r}"
    return True, f"{source.bytes} bytes over the tcp relay"
