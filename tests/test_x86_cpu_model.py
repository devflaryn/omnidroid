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
    def test_a_named_model_is_the_default_on_whpx_and_kvm(self):
        # CORRECTION 2026-09-02: qemu64 really is what the guest sees under
        # WHPX (no XSAVE, no AVX -- `dmesg`: "x87 FPU will use FXSAVE"), and
        # +avx on qemu64 breaks XSAVE and kills Roblox at start. A named
        # model carries a consistent CPUID; Skylake-Client-v4 measured
        # 38/38 -> 42/47 fps in PS99. `host` is still the measured-bad one.
        for accel in ("whpx,kernel-irqchip=off", "kvm"):
            self.assertEqual(qemu_proc.x86_cpu_model(accel),
                             qemu_proc.WHPX_KVM_CPU_MODEL, accel)
        self.assertEqual(qemu_proc.WHPX_KVM_CPU_MODEL, "Skylake-Client-v4")

    def test_other_hypervisors_keep_the_baseline(self):
        # `+aes` is not optional -- libndk_translation ASSERTS on a host
        # without AES-NI the first time Roblox touches AES.
        self.assertEqual(qemu_proc.x86_cpu_model("hvf"), "qemu64,+aes")

    def test_env_override_wins_for_an_ab(self):
        import os
        os.environ["OMNI_CPU"] = "Haswell-v4"
        try:
            self.assertEqual(qemu_proc.x86_cpu_model("whpx"), "Haswell-v4")
        finally:
            del os.environ["OMNI_CPU"]

    def test_config_can_pin_any_model(self):
        cfg = {"qemu": {"cpu": "Skylake-Client-v4"}}
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", cfg),
                         "Skylake-Client-v4")

    def test_a_config_without_a_qemu_block_is_fine(self):
        m = qemu_proc.WHPX_KVM_CPU_MODEL
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {}), m)
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", {"qemu": None}), m)
        self.assertEqual(qemu_proc.x86_cpu_model("whpx", None), m)


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
