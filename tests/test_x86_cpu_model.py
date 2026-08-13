# omnidroid/tests/test_x86_cpu_model.py
"""x86 guest sizing and CPU model on a hypervisor -- both settled by
MEASUREMENT on a Windows/WHPX host (i7-13700F, 32 GB, Bliss 16.9.7,
Android 13), because both look obvious the wrong way round.

    RAM / vCPU     -cpu       time to boot_completed
    8192 / 8       qemu64     5.9 min
    8192 / 8       host       did not complete (6 min timeout)
    2048 / 2       host       did not complete (15 min timeout)
    4096 / 4       host       did not complete (15 min timeout)
    4096 / 4       qemu64     0.9 min

Two findings, neither of which survives reasoning alone:

 1. `-cpu host` is WORSE here, not better -- three runs, zero completions --
    even though the guest's whole job is translating arm64 through
    libndk_translation, which is exactly the workload you would expect to
    benefit from real SSE4/AVX.
 2. Autoscaling to the host's capacity makes boots 6.5x SLOWER under WHPX.
    More resources, much worse result.
"""
import unittest
from unittest import mock

from omnidroid import qemu_proc


class CpuModel(unittest.TestCase):
    def test_the_baseline_model_is_the_default(self):
        # Measured: `host` did not complete a single boot on WHPX.
        for accel in ("whpx,kernel-irqchip=off", "kvm", "hvf", "tcg"):
            self.assertEqual(qemu_proc.x86_cpu_model(accel), "qemu64", accel)

    def test_config_can_still_ask_for_host(self):
        cfg = {"qemu": {"cpu": "host"}}
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", cfg), "host")

    def test_config_can_pin_any_model(self):
        cfg = {"qemu": {"cpu": "Skylake-Client-v4"}}
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", cfg),
                         "Skylake-Client-v4")

    def test_a_config_without_a_qemu_block_is_fine(self):
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {}), "qemu64")
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {"qemu": None}),
                         "qemu64")
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", None), "qemu64")


class WhpxCeilings(unittest.TestCase):
    """WHPX's per-vCPU exit cost means growing an instance to the host's
    capacity makes it far SLOWER to boot -- the opposite of what autoscaling
    is for. Capped on Windows only; KVM and HVF scale as expected."""

    def _scaled(self, windows, host_mem=32768, host_cpus=16):
        mode = {"name": "playable", "autoscale": True, "mem": 4096, "smp": 4}
        with mock.patch.object(qemu_proc, "IS_WINDOWS", windows):
            return qemu_proc.autoscale_perf(mode, host_mem, host_cpus)

    def test_windows_is_capped(self):
        m = self._scaled(windows=True)
        self.assertLessEqual(m["smp"], qemu_proc.WHPX_SMP_CEIL)
        self.assertLessEqual(m["mem"], qemu_proc.WHPX_MEM_CEIL_MB)

    def test_a_big_windows_host_does_not_get_8_vcpu(self):
        # The regression: 16 logical CPUs used to yield smp 8, which measured
        # 5.9 min against 0.9 min at smp 4.
        self.assertEqual(self._scaled(windows=True)["smp"], 4)

    def test_other_platforms_still_scale_up(self):
        m = self._scaled(windows=False)
        self.assertGreater(m["smp"], qemu_proc.WHPX_SMP_CEIL)
        self.assertGreater(m["mem"], qemu_proc.WHPX_MEM_CEIL_MB)

    def test_a_non_autoscaling_mode_is_untouched(self):
        fixed = {"name": "brutal", "mem": 2048, "smp": 2}
        with mock.patch.object(qemu_proc, "IS_WINDOWS", True):
            self.assertEqual(qemu_proc.autoscale_perf(fixed, 32768, 16), fixed)

    def test_an_unreadable_host_costs_the_upgrade_not_the_boot(self):
        mode = {"name": "playable", "autoscale": True, "mem": 4096, "smp": 4}
        with mock.patch.object(qemu_proc, "IS_WINDOWS", True):
            m = qemu_proc.autoscale_perf(mode, None, None)
        self.assertEqual(m["mem"], 4096)
        self.assertEqual(m["smp"], 4)


if __name__ == "__main__":
    unittest.main()
