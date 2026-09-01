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
    def test_the_baseline_model_is_the_default_on_a_hypervisor(self):
        # Measured: `host` did not complete a single boot on WHPX. `+aes` is
        # not optional -- libndk_translation ASSERTS on a host without AES-NI
        # the first time Roblox touches AES, and the splash dies two seconds in.
        for accel in ("whpx,kernel-irqchip=off", "kvm", "hvf"):
            self.assertEqual(qemu_proc.x86_cpu_model(accel), "qemu64,+aes",
                             accel)

    def test_tcg_gets_max_because_the_baseline_will_not_boot_on_it(self):
        """MEASURED 2026-08-21, same image, kernel log on ttyS0:

            -accel tcg -cpu qemu64,+aes   0 bytes in 240 s -- the kernel never
                                          printed line one
            -accel tcg -cpu max           Android at bootcomplete in 104 s

        From outside, the first case is indistinguishable from a very slow
        boot: QEMU burns 99 % of a core. The tell is that it does ZERO disk
        I/O. Under WHPX the baseline survives only because WHPX's CPUID
        filtering is limited, so the mask was never really being applied."""
        for accel in ("tcg", "tcg,thread=multi", "TCG"):
            self.assertEqual(qemu_proc.x86_cpu_model(accel), "max", accel)

    def test_an_explicit_config_still_wins_even_on_tcg(self):
        self.assertEqual(
            qemu_proc.x86_cpu_model("tcg", {"qemu": {"cpu": "Skylake-Client"}}),
            "Skylake-Client")

    def test_config_can_still_ask_for_host(self):
        cfg = {"qemu": {"cpu": "host"}}
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", cfg), "host")

    def test_config_can_pin_any_model(self):
        cfg = {"qemu": {"cpu": "Skylake-Client-v4"}}
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", cfg),
                         "Skylake-Client-v4")

    def test_a_config_without_a_qemu_block_is_fine(self):
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {}), "qemu64,+aes")
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {"qemu": None}),
                         "qemu64,+aes")
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", None), "qemu64,+aes")


class WhpxCeilings(unittest.TestCase):
    """WHPX gets its own ceilings because it does not scale like KVM/HVF.

    The MEMORY cap is a commit-charge fact (Windows charges `-m` 1:1 and a
    host pushed into its pagefile misses vCPU deadlines). The vCPU cap was
    4 until 2026-09-01 on the strength of a reading that moved memory and
    vCPUs in one step; re-measured with memory held at 4096, smp 8 boots in
    34.6 s against smp 4's 33.6 s and gives the guest ~5x one vCPU's
    throughput instead of ~3.1x. See WHPX_SMP_CEIL."""

    def _scaled(self, windows, host_mem=32768, host_cpus=16):
        mode = {"name": "playable", "autoscale": True, "mem": 4096, "smp": 4}
        with mock.patch.object(qemu_proc, "IS_WINDOWS", windows):
            return qemu_proc.autoscale_perf(mode, host_mem, host_cpus)

    def test_windows_is_capped(self):
        m = self._scaled(windows=True)
        self.assertLessEqual(m["smp"], qemu_proc.WHPX_SMP_CEIL)
        self.assertLessEqual(m["mem"], qemu_proc.WHPX_MEM_CEIL_MB)

    def test_a_big_windows_host_gets_the_whole_whpx_ceiling(self):
        """16 logical CPUs yield the ceiling, not the old hardcoded 4.

        Roblox alone was measured using 297% of a core out of the 400% a
        4-vCPU guest has -- starved, not sated -- and 8 vCPUs cost nothing
        at the boot (34.6 s vs 33.6 s, memory held constant)."""
        self.assertEqual(self._scaled(windows=True)["smp"],
                         qemu_proc.WHPX_SMP_CEIL)

    def test_a_small_windows_host_still_keeps_cores_for_itself(self):
        """The ceiling is a CEILING. A 6-core host gets 4, not 8:
        PERF_SMP_HOST_RESERVE is still what decides on a small machine."""
        self.assertEqual(self._scaled(windows=True, host_cpus=6)["smp"], 4)

    def test_other_platforms_still_scale_up(self):
        """The MEMORY ceiling is the Windows-only one now: KVM/HVF have
        madvise and a balloon that decommits, so they take the bigger guest.
        The vCPU ceilings happen to coincide at 8 (PERF_SMP_CEIL), which is
        why only memory is asserted here."""
        m = self._scaled(windows=False)
        self.assertGreater(m["mem"], qemu_proc.WHPX_MEM_CEIL_MB)
        self.assertEqual(m["smp"], qemu_proc.PERF_SMP_CEIL)

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
