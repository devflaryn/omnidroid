"""macOS rows: the gfx workstream (MoltenVK). Pure data; see `__init__.py`.

Run on the macOS host with `python3 tools/mutate.py --only mac-gfx-`. The command runs every omni-gfx
test, the gated live ones included (a window, the GPU), in release for the reason `window.py` gives.

**Two fixes have no row, because nothing on this host can see them**, and a row would only report
NOT CAUGHT: enabling `VK_KHR_portability_subset` itself, and chaining the subset's supported
features into `vkCreateDevice`. MoltenVK accepts a device without either, and there is no validation
layer installed here to report the violation (MEASURED: `vulkaninfo` lists no layers). The subset's
*reading* is pinned (`mac-gfx-A3`, `portability_live.rs`); its *enabling* is a specification
requirement kept by reading the code, and is recorded in `docs/ports/macos-window.md` as such.
"""

_PORT = "crates/omni-gfx/src/portability.rs"
_VULKAN = "crates/omni-gfx/src/vulkan.rs"
_HOST = "crates/omni-gfx/src/host.rs"

_GFX = ["env", "OMNI_GFX_WINDOW_TESTS=1", "cargo", "test", "-p", "omni-gfx", "--release",
        "--no-fail-fast", "--", "--include-ignored", "--test-threads=1"]

ROWS = [
    ("mac-gfx-A1", "A", "the platform's loader locations ignored (Entry::load() only)", _PORT,
     """    if candidates.is_empty() {""",
     """    if true || candidates.is_empty() {""",
     _GFX),

    ("mac-gfx-A2", "A", "never retried with portability enumeration", _PORT,
     """    failure == vk::Result::ERROR_INCOMPATIBLE_DRIVER && offered.contains(&ENUMERATION)""",
     """    false && failure == vk::Result::ERROR_INCOMPATIBLE_DRIVER && offered.contains(&ENUMERATION)""",
     _GFX),

    ("mac-gfx-A3", "A", "a 1.0 instance not given VK_KHR_get_physical_device_properties2", _PORT,
     """        add.push(PROPERTIES2);""",
     """        let _ = PROPERTIES2;""",
     _GFX),

    ("mac-gfx-A4", "A", "the renderer's retry without the enumerate-portability flag", _VULKAN,
     """                flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;""",
     """                flags |= vk::InstanceCreateFlags::empty();""",
     _GFX),

    ("mac-gfx-A5", "A", "the host's retry without the enumerate-portability flag", _HOST,
     """                    flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;""",
     """                    flags |= vk::InstanceCreateFlags::empty();""",
     _GFX),

    ("mac-gfx-A6", "A", "a minimised window presented to because the surface still has a size", _VULKAN,
     """        let minimised = self.target_extent.0 == 0 || self.target_extent.1 == 0;""",
     """        let minimised = false;""",
     _GFX),

    ("mac-gfx-A7", "A", "the renderer's Metal surface made without the layer", _VULKAN,
     """        RawWindow::AppKit { ca_metal_layer, .. } => {
            let info = vk::MetalSurfaceCreateInfoEXT::default()
                .layer(ca_metal_layer as *const vk::CAMetalLayer);""",
     """        RawWindow::AppKit { ca_metal_layer, .. } => {
            let _ = ca_metal_layer;
            let info = vk::MetalSurfaceCreateInfoEXT::default().layer(core::ptr::null());""",
     _GFX),

    ("mac-gfx-A8", "A", "the host's Metal surface made without the layer", _HOST,
     """                let info = vk::MetalSurfaceCreateInfoEXT::default()
                    .layer(ca_metal_layer as *const vk::CAMetalLayer);""",
     """                let _ = ca_metal_layer;
                let info = vk::MetalSurfaceCreateInfoEXT::default().layer(core::ptr::null());""",
     _GFX),

    ("mac-gfx-A9", "A", "the host reports the Win32 entry point for a Metal surface", _HOST,
     """                            host_call: PLATFORM_SURFACE_ENTRY_POINTS[4].to_string(),""",
     """                            host_call: PLATFORM_SURFACE_ENTRY_POINTS[0].to_string(),""",
     _GFX),
]
