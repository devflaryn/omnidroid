//! **What the host's Vulkan device leaves out, named and checked** -- the portability subset.
//!
//! A portability implementation (MoltenVK) exposes `VK_KHR_portability_subset` and describes what
//! it does not support in `VkPhysicalDevicePortabilitySubsetFeaturesKHR`. The renderer enables
//! what it supports and reports the rest as [`DeviceReport::portability_gaps`]. This test reads the
//! same structure **independently** -- its own instance, its own query, straight through `ash` --
//! and asserts the report names exactly the members that read false: membership both ways, by name
//! (VERIFICATION entry 1). On a native driver both sides must say "not a portability device".
//!
//! It prints the list, which is the record `docs/ports/macos-window.md` quotes.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-gfx --release --test portability_live -- --ignored --nocapture
//! ```

use ash::vk;
use omni_gfx::vulkan::{Renderer, RendererConfig};
use omni_platform::window::{Window, WindowDesc};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Every member of the subset structure with the specification's name for it.
fn named(f: &vk::PhysicalDevicePortabilitySubsetFeaturesKHR<'_>) -> Vec<(&'static str, bool)> {
    vec![
        ("constantAlphaColorBlendFactors", f.constant_alpha_color_blend_factors != 0),
        ("events", f.events != 0),
        ("imageViewFormatReinterpretation", f.image_view_format_reinterpretation != 0),
        ("imageViewFormatSwizzle", f.image_view_format_swizzle != 0),
        ("imageView2DOn3DImage", f.image_view2_d_on3_d_image != 0),
        ("multisampleArrayImage", f.multisample_array_image != 0),
        ("mutableComparisonSamplers", f.mutable_comparison_samplers != 0),
        ("pointPolygons", f.point_polygons != 0),
        ("samplerMipLodBias", f.sampler_mip_lod_bias != 0),
        ("separateStencilMaskRef", f.separate_stencil_mask_ref != 0),
        ("shaderSampleRateInterpolationFunctions", f.shader_sample_rate_interpolation_functions != 0),
        ("tessellationIsolines", f.tessellation_isolines != 0),
        ("tessellationPointMode", f.tessellation_point_mode != 0),
        ("triangleFans", f.triangle_fans != 0),
        ("vertexAttributeAccessBeyondStride", f.vertex_attribute_access_beyond_stride != 0),
    ]
}

/// The subset features of the first device, read on a 1.1 instance of our own; `None` when the
/// device is not a portability implementation.
fn independent_reading() -> Option<Vec<(&'static str, bool)>> {
    let candidates = omni_platform::window::vulkan_loader_candidates();
    // SAFETY: loading the host's Vulkan loader, from the same candidates the renderer uses.
    let entry = if candidates.is_empty() {
        unsafe { ash::Entry::load() }.ok()
    } else {
        candidates.iter().find_map(|path| unsafe { ash::Entry::load_from(path) }.ok())
    }
    .expect("a Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    // SAFETY: takes no handles.
    let offered = unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap();
    let portability = offered.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::khr::portability_enumeration::NAME));
    let names = [ash::khr::portability_enumeration::NAME.as_ptr()];
    let mut info = vk::InstanceCreateInfo::default().application_info(&app);
    if portability {
        info = info.enabled_extension_names(&names).flags(vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR);
    }
    // SAFETY: every pointer in `info` outlives the call.
    let instance = unsafe { entry.create_instance(&info, None) }.unwrap();
    // SAFETY: the instance is live until destroyed below; the device is its own.
    let reading = unsafe {
        let physical = instance.enumerate_physical_devices().unwrap()[0];
        let extensions = instance.enumerate_device_extension_properties(physical).unwrap();
        if extensions.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::khr::portability_subset::NAME)) {
            let mut subset = vk::PhysicalDevicePortabilitySubsetFeaturesKHR::default();
            let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut subset);
            instance.get_physical_device_features2(physical, &mut features);
            Some(named(&subset))
        } else {
            None
        }
    };
    // SAFETY: nothing was created from it.
    unsafe { instance.destroy_instance(None) };
    reading
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_renderer_names_exactly_the_subset_features_the_device_lacks() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "run with --ignored but {GATE} is not 1; this opens a window and the GPU"
    );
    let mut window = Window::new(&WindowDesc::new("omnidroid: portability", 320, 240)).unwrap();
    let _ = window.poll_events().count();
    let renderer = Renderer::new(window.raw(), window.client_size().unwrap(), RendererConfig::default()).unwrap();
    let report = renderer.device().clone();
    let reading = independent_reading();
    println!("{} ({:?}): portability gaps {:?}", report.name, report.device_type, report.portability_gaps);
    match (&report.portability_gaps, reading) {
        (None, None) => {}
        (Some(gaps), Some(reading)) => {
            let lacking: Vec<&str> = reading.iter().filter(|(_, has)| !has).map(|(name, _)| *name).collect();
            assert_eq!(gaps, &lacking, "the report and an independent reading disagree; all: {reading:?}");
        }
        (report, reading) => panic!("the renderer says {report:?} and an independent reading says {reading:?}"),
    }
    drop(renderer);
}
