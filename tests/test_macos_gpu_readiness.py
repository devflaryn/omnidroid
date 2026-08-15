#!/usr/bin/env python3
"""macOS says which of its three prerequisites are missing.

    python3 -m pytest tests/test_macos_gpu_readiness.py -q

None of the three is code in this repo, and all three fail the same way from
the user's chair -- a black or 3 fps guest -- so the product has to name them:

  1. a virgl-capable QEMU (Homebrew core has no virtio-gpu-gl-pci at all)
  2. an arm base rebuilt with ro.hardware.egl=mesa (it ships `angle`, so guest
     GL goes ANGLE -> SwiftShader in software whatever the host offers, and
     ro.* is immutable after init)
  3. Xcode CLT new enough for Homebrew to build from source

The SurfaceFlinger line is the acceptance test for (2): want `Mesa, virgl`,
never `ANGLE ... SwiftShader`.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import macgpu

HAS_GL_DEVICE = 'name "virtio-gpu-gl-pci", bus PCI\n'
NO_GL_DEVICE = 'name "virtio-gpu-pci", bus PCI\n'
MESA = "GLES: Mesa, virgl (Apple M1), OpenGL ES 3.2"
ANGLE = "GLES: ANGLE (Apple, Apple M1, SwiftShader), OpenGL ES 3.0"


class Readiness(unittest.TestCase):

    def test_no_gl_device_is_reported_as_the_qemu_build(self):
        result = macgpu.readiness(qemu_device_help=NO_GL_DEVICE)
        self.assertFalse(result["ready"])
        self.assertTrue(any("virglrenderer" in b for b in result["blockers"]))

    def test_angle_in_surfaceflinger_is_reported_as_the_image(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles=ANGLE)
        self.assertFalse(result["ready"])
        self.assertTrue(any("ro.hardware.egl" in b
                            for b in result["blockers"]))

    def test_both_present_is_ready(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles=MESA)
        self.assertTrue(result["ready"], result["blockers"])
        self.assertEqual(result["blockers"], [])

    def test_an_unknown_surfaceflinger_line_is_not_treated_as_a_pass(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles="")
        self.assertFalse(result["ready"])


if __name__ == "__main__":
    unittest.main()
