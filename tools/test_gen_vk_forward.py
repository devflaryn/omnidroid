#!/usr/bin/env python3
"""Tests of tools/gen_vk_forward.py: python -m unittest tools/test_gen_vk_forward.py (or run it)."""
import os
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import gen_vk_forward as gen  # noqa: E402

XML = os.path.join(HERE, "vk", "vk.xml")
SPECIAL = os.path.join(HERE, "vk", "special.txt")


def read(root, rel):
    with open(os.path.join(root, rel), encoding="utf-8", newline="") as f:
        return f.read()


class Generated(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="gen_vk_forward_")
        cls.sel = gen.generate(XML, SPECIAL, cls.tmp, quiet=True)
        cls.rs = read(cls.tmp, gen.HOST_OUT)
        cls.h = read(cls.tmp, gen.GUEST_H_OUT)
        cls.c = read(cls.tmp, gen.GUEST_C_OUT)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def fn_rs(self, ident):
        m = re.search(r"\nfn %s\(.*?\n}\n" % ident, self.rs, re.S)
        self.assertIsNotNone(m, ident)
        return m.group(0)

    def fn_c(self, name):
        m = re.search(r"\nstatic VKAPI_ATTR [^\n]* VKAPI_CALL omni_%s\(.*?\n}\n" % name, self.c, re.S)
        self.assertIsNotNone(m, name)
        return m.group(0)

    def test_float_parameter_travels_as_its_bits(self):
        rs = self.fn_rs("vk_cmd_set_line_width")
        self.assertIn('unsafe extern "system" fn(u64, f32) =', rs)
        self.assertIn("f(h0, f32::from_bits(a[1] as u32))", rs)
        c = self.fn_c("vkCmdSetLineWidth")
        self.assertIn("(VkCommandBuffer commandBuffer, float lineWidth)", c)
        self.assertIn("memcpy(&omni_bits1, &lineWidth, sizeof omni_bits1);", c)
        self.assertIn("omni_a[1] = omni_bits1;", c)
        self.assertIn("omni_vk_call(OMNI_VK_ID_VK_CMD_SET_LINE_WIDTH, omni_a, 2)", c)

    def test_dispatchable_first_parameter_and_u32s(self):
        rs = self.fn_rs("vk_cmd_draw")
        self.assertIn('const NAMES: &[&CStr] = &[c"vkCmdDraw"];', rs)
        self.assertIn("let (h0, t) = g.dispatchable(p, a[0])?;", rs)
        self.assertIn('unsafe extern "system" fn(u64, u32, u32, u32, u32) =', rs)
        self.assertIn("t.get(ID_VK_CMD_DRAW, NAMES)?", rs)
        self.assertIn("f(h0, a[1] as u32, a[2] as u32, a[3] as u32, a[4] as u32)", rs)
        self.assertIn("ID_VK_CMD_DRAW => vk_cmd_draw(g, p, a),", self.rs)
        c = self.fn_c("vkCmdDraw")
        self.assertIn("omni_a[0] = (uint64_t)(uintptr_t)commandBuffer;", c)
        self.assertIn("omni_a[1] = (uint64_t)vertexCount;", c)

    def test_integer_widths(self):
        # VkPipelineStageFlags2 is VkFlags64; vertexOffset is int32_t; the stipple pattern uint16_t.
        self.assertIn('fn(u64, u64, u64, u32) =', self.fn_rs("vk_cmd_write_timestamp2"))
        self.assertIn("f(h0, a[1] as u32, a[2] as u32, a[3] as u32, a[4] as u32 as i32, a[5] as u32)",
                      self.fn_rs("vk_cmd_draw_indexed"))
        self.assertIn("f(h0, a[1] as u32, a[2] as u16)", self.fn_rs("vk_cmd_set_line_stipple_ext"))

    def test_result_and_64_bit_returns(self):
        rs = self.fn_rs("vk_wait_for_fences")
        self.assertIn("fn(u64, u32, u64, u32, u64) -> i32 =", rs)
        self.assertIn("Ok(u64::from(r as u32))", rs)
        self.assertIn("return (VkResult)(int32_t)(uint32_t)omni_r;", self.fn_c("vkWaitForFences"))
        self.assertIn("Ok(r)", self.fn_rs("vk_get_buffer_device_address"))

    def test_special_command_is_a_prototype_and_a_table_entry_only(self):
        self.assertIn("VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateInstance(const VkInstanceCreateInfo* pCreateInfo, "
                      "const VkAllocationCallbacks* pAllocator, VkInstance* pInstance);", self.h)
        self.assertNotIn("omni_vkCreateInstance(", self.c)
        self.assertIn('{"vkCreateInstance", (PFN_vkVoidFunction)omni_vkCreateInstance, 0},', self.c)
        self.assertIn('("vkCreateInstance", 3, true),', self.rs)
        self.assertNotIn("fn vk_create_instance(", self.rs)
        self.assertNotIn("ID_VK_CREATE_INSTANCE =>", self.rs)
        # An alias of a special name is special too.
        self.assertIn('{"vkGetPhysicalDeviceProperties2KHR", (PFN_vkVoidFunction)omni_vkGetPhysicalDeviceProperties2, 1},', self.c)

    def test_alias_points_at_its_command(self):
        self.assertIn('{"vkCmdBeginRenderingKHR", (PFN_vkVoidFunction)omni_vkCmdBeginRendering, 2},', self.c)
        self.assertIn('{"vkCmdBeginRendering", (PFN_vkVoidFunction)omni_vkCmdBeginRendering, 2},', self.c)
        self.assertNotIn("omni_vkCmdBeginRenderingKHR", self.c)
        self.assertNotIn("ID_VK_CMD_BEGIN_RENDERING_KHR", self.rs)
        self.assertIn('&[c"vkCmdBeginRendering", c"vkCmdBeginRenderingKHR"]', self.fn_rs("vk_cmd_begin_rendering"))

    def test_platform_and_excluded_extensions_absent(self):
        for text in (self.rs, self.h, self.c):
            self.assertNotIn("Win32", text)
            self.assertNotIn("vkCreateSwapchainKHR", text)
            self.assertNotIn("vkDebugReportMessageEXT", text)
            self.assertNotIn("vkGetMemoryFdKHR", text)
        for name in ("VK_KHR_external_memory_win32", "VK_KHR_swapchain", "VK_KHR_surface", "VK_EXT_debug_utils",
                     "VK_KHR_external_memory_fd", "VK_LUNARG_direct_driver_loading"):
            self.assertNotIn(f'("{name}",', self.rs)
        for name, device in (("VK_ANDROID_external_memory_android_hardware_buffer", "true"),
                             ("VK_KHR_external_semaphore_fd", "true"), ("VK_KHR_external_fence_fd", "true"),
                             ("VK_KHR_get_physical_device_properties2", "false")):
            self.assertIn(f'("{name}", {device}),', self.rs)

    def test_ids_dense_and_tables_sorted(self):
        ids = [int(i) for i in re.findall(r"pub\(crate\) const ID_\w+: u32 = (\d+);", self.rs)]
        self.assertEqual(ids, list(range(len(ids))))
        commands = re.findall(r'^    \("(vk\w+)", \d+, (?:true|false)\),$', self.rs, re.M)
        self.assertEqual(len(commands), len(ids))
        self.assertEqual(commands, sorted(commands))
        names = re.findall(r'^    \{"(vk\w+)", ', self.c, re.M)
        self.assertEqual(names, sorted(names, key=lambda s: s.encode()))
        self.assertIn(f"const unsigned omni_vk_entry_count = {len(names)}u;", self.c)
        exts = re.findall(r'^    \("(VK_\w+)", (?:true|false)\),$', self.rs, re.M)
        self.assertEqual(exts, sorted(exts))
        for i, name in enumerate(commands):
            self.assertIn(f"#define OMNI_VK_ID_{gen.screaming(name)} {i}u", self.h)

    def test_deterministic(self):
        other = tempfile.mkdtemp(prefix="gen_vk_forward_")
        try:
            gen.generate(XML, SPECIAL, other, quiet=True)
            for rel in (gen.HOST_OUT, gen.GUEST_H_OUT, gen.GUEST_C_OUT):
                with open(os.path.join(self.tmp, rel), "rb") as a, open(os.path.join(other, rel), "rb") as b:
                    self.assertEqual(a.read(), b.read(), rel)
        finally:
            shutil.rmtree(other, ignore_errors=True)


class Refuses(unittest.TestCase):
    """The generator fails, naming the command, rather than forward what it cannot."""

    def run_with_special(self, names):
        tmp = tempfile.mkdtemp(prefix="gen_vk_forward_")
        try:
            path = os.path.join(tmp, "special.txt")
            with open(path, "w", encoding="utf-8") as f:
                f.write("\n".join(names) + "\n")
            gen.generate(XML, path, os.path.join(tmp, "out"), quiet=True)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_a_command_taking_dispatchable_handles_must_be_special(self):
        names = [n for n in gen.read_special(SPECIAL) if n != "vkQueueSubmit"]
        with self.assertRaisesRegex(gen.GenError, r"vkQueueSubmit: .*dispatchable"):
            self.run_with_special(names)

    def test_a_dispatchable_parameter_must_be_special(self):
        names = [n for n in gen.read_special(SPECIAL) if n != "vkGetDeviceQueue"]
        with self.assertRaisesRegex(gen.GenError, r"vkGetDeviceQueue: parameter pQueue .* dispatchable handle"):
            self.run_with_special(names)

    def test_a_global_command_must_be_special(self):
        names = [n for n in gen.read_special(SPECIAL) if n != "vkEnumerateInstanceVersion"]
        with self.assertRaisesRegex(gen.GenError, r"vkEnumerateInstanceVersion: a global command"):
            self.run_with_special(names)

    def test_an_android_command_must_be_special(self):
        names = [n for n in gen.read_special(SPECIAL) if n != "vkGetMemoryAndroidHardwareBufferANDROID"]
        with self.assertRaisesRegex(gen.GenError, r"vkGetMemoryAndroidHardwareBufferANDROID: .*must be special"):
            self.run_with_special(names)

    def test_a_special_name_must_be_forwarded(self):
        with self.assertRaisesRegex(gen.GenError, r"vkCreateWin32SurfaceKHR"):
            self.run_with_special(gen.read_special(SPECIAL) + ["vkCreateWin32SurfaceKHR"])


if __name__ == "__main__":
    unittest.main()
