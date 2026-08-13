# omnidroid/tests/test_port_probe.py
"""Free-port detection must not depend on how the host treats a closed port.

REGRESSION (Windows, 2026-08-13). `_port_answers` used to infer "free" from
ConnectionRefusedError — i.e. it assumed the host answers a SYN to a closed
loopback port with an RST. Windows boxes running certain endpoint-security
filters silently DROP those SYNs instead. Every probe then timed out, every
timeout resolved to "occupied" (the deliberately safe direction), and
`allocate_ports`'s unbounded loop walked port indices forever: `omni start`
hung with zero output, no QEMU process and no timeout, on a host whose
`version`/`doctor`/`bases` all passed.

A bind test is authoritative on every platform — the kernel either grants the
port or refuses it — so these tests hold whichever way the host treats a
closed port.
"""
import socket
import unittest

from omnidroid import runtime


def _cfg(**kw):
    q = {"adb_port_start": 16001, "qmp_port_start": 17001,
         "vnc_port_start": 18001}
    q.update(kw)
    return {"qemu": q}


class PortAnswers(unittest.TestCase):
    def test_a_listening_port_reads_as_occupied(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        port = s.getsockname()[1]
        try:
            self.assertTrue(runtime._port_answers(port))
        finally:
            s.close()

    def test_a_closed_port_reads_as_free(self):
        # THE regression. Bind-and-release to get a port nothing holds, then
        # assert the probe calls it free even on a host that blackholes the
        # connect() a previous implementation relied on.
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        self.assertFalse(runtime._port_answers(port))

    def test_the_timeout_kwarg_is_still_accepted(self):
        # Callers and several tests monkeypatch/call it as
        # _port_answers(port, timeout=0.25); the signature must not break.
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        self.assertFalse(runtime._port_answers(port, timeout=0.25))


class AllocatePorts(unittest.TestCase):
    def test_it_returns_an_aligned_triple_and_terminates(self):
        cfg = _cfg()
        adb, qmp, vnc = runtime.allocate_ports(cfg)
        self.assertEqual(adb - 16001, qmp - 17001)
        self.assertEqual(adb - 16001, vnc - 18001)

    def test_it_fails_fast_instead_of_spinning_forever(self):
        # With every port reading as occupied (the exact state the blackholed
        # host produced), allocation must raise rather than loop to infinity.
        orig = runtime._port_answers
        runtime._port_answers = lambda port, timeout=0.25: True
        try:
            with self.assertRaises(Exception) as ctx:
                runtime.allocate_ports(_cfg())
            self.assertIn("port", str(ctx.exception).lower())
        finally:
            runtime._port_answers = orig


if __name__ == "__main__":
    unittest.main()
