"""macOS rows: omni-android behaviour the macOS host exposed (shared code, listed in
docs/ports/macos.md's merge notes). Pure data; see `__init__.py`."""

SWAPCHAIN = "crates/omni-android/src/vulkan/swapchain.rs"
VULKAN_PRESENT = ["cargo", "test", "-p", "omni-android", "--release", "--test", "vulkan_present",
                  "--no-fail-fast"]

ROWS = [
    ("mac-and-A1", "A", "VK_SUBOPTIMAL_KHR from acquire reaches the guest verbatim, which Android never "
     "answers there: on MoltenVK the first resize killed the render thread",
     SWAPCHAIN,
     """    let answer = if acquired.result == VK_SUBOPTIMAL_KHR { VK_SUCCESS } else { acquired.result };""",
     """    let answer = acquired.result;""",
     VULKAN_PRESENT),
]
