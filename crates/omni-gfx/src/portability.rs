//! **Finding the loader, and Vulkan portability implementations** (MoltenVK): the three things a
//! host whose driver is a translation layer needs that a native driver does not, done generically
//! -- nothing here names a platform, and each step changes nothing on a host that does not need it.
//!
//! 1. **The loader's location.** [`load_entry`] asks `omni_platform` where the loader may be
//!    ([`vulkan_loader_candidates`](omni_platform::window::vulkan_loader_candidates)). An empty
//!    answer -- Windows, Linux -- is exactly `ash::Entry::load()`, as before. A list is tried in
//!    order with `Entry::load_from`, and when nothing loads, the error names **every** path tried
//!    with what the dynamic loader said about it.
//! 2. **Enumeration.** Since Vulkan loader 1.3.216, a *portability* driver (one that implements a
//!    subset of Vulkan over another API) is enumerated only for an instance created with
//!    `VK_KHR_portability_enumeration` and `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR`;
//!    without them, a host whose only driver is one gets `VK_ERROR_INCOMPATIBLE_DRIVER` from
//!    `vkCreateInstance` (MEASURED on this project's macOS host: Homebrew loader 1.4.357 + MoltenVK
//!    1.4.2). [`retry_with_portability`] says when to try again with both: **only** after exactly
//!    that failure, and only when the loader offers the extension. An instance that is created the
//!    first time is created exactly as it always was -- so a Windows machine with a native driver,
//!    whose loader also offers the extension, is not touched.
//! 3. **The subset.** A device that exposes `VK_KHR_portability_subset` **must** have it enabled
//!    (Vulkan spec, `VK_KHR_portability_subset`: "an application must enable it"), and what it
//!    leaves out is described by `VkPhysicalDevicePortabilitySubsetFeaturesKHR`. [`Subset`] queries
//!    that structure, enables exactly what the device supports -- nothing is claimed that it does
//!    not have -- and [`Subset::gaps`] names what is missing, for a report. No native driver
//!    exposes the extension, so on one this is a no-op.

use std::ffi::CStr;

use ash::vk;

/// Load the Vulkan loader: `ash::Entry::load()` when the platform names no candidates, otherwise
/// each candidate in order. See this module's point 1.
///
/// # Errors
///
/// The text for [`GfxError::LoaderMissing`](crate::GfxError::LoaderMissing): what the dynamic
/// loader said, for every path tried.
pub(crate) fn load_entry() -> Result<ash::Entry, String> {
    let candidates = omni_platform::window::vulkan_loader_candidates();
    if candidates.is_empty() {
        // SAFETY: `Entry::load` dlopens the platform's Vulkan loader; it is unsafe because the
        // library is arbitrary host code, and there is no way to make loading a driver safe.
        return unsafe { ash::Entry::load() }.map_err(|err| err.to_string());
    }
    let mut tried = Vec::with_capacity(candidates.len());
    for path in candidates {
        // SAFETY: as above, for a path the platform named.
        match unsafe { ash::Entry::load_from(path) } {
            Ok(entry) => return Ok(entry),
            Err(err) => tried.push(format!("`{path}`: {err}")),
        }
    }
    Err(format!("no Vulkan loader loaded from any of the {} places tried, in order: {}", tried.len(), tried.join("; ")))
}

/// `VK_KHR_portability_enumeration`.
pub(crate) const ENUMERATION: &CStr = ash::khr::portability_enumeration::NAME;
/// `VK_KHR_get_physical_device_properties2`, which `VK_KHR_portability_subset` requires on a 1.0
/// instance and which is how its features are read there.
pub(crate) const PROPERTIES2: &CStr = ash::khr::get_physical_device_properties2::NAME;
/// `VK_KHR_portability_subset`.
pub(crate) const SUBSET: &CStr = ash::khr::portability_subset::NAME;

/// Whether an instance creation that failed with `failure` should be retried with portability
/// enumeration: the failure is the loader's "no non-portability driver", the loader offers the
/// extension, and the request did not already ask for it. See this module's point 2.
pub(crate) fn retry_with_portability(failure: vk::Result, offered: &[&CStr], requested: &[&CStr]) -> bool {
    failure == vk::Result::ERROR_INCOMPATIBLE_DRIVER && offered.contains(&ENUMERATION) && !requested.contains(&ENUMERATION)
}

/// The instance-extension names to add for a portability retry, given what the request already
/// has and what the loader offers: the enumeration extension, and -- for an instance whose API
/// version is below 1.1 -- `VK_KHR_get_physical_device_properties2` when offered, so that the
/// subset's features can be read and its dependency is met.
pub(crate) fn portability_additions(offered: &[&CStr], requested: &[&CStr], api_version: u32) -> Vec<&'static CStr> {
    let mut add = vec![ENUMERATION];
    if api_version < vk::API_VERSION_1_1 && offered.contains(&PROPERTIES2) && !requested.contains(&PROPERTIES2) {
        add.push(PROPERTIES2);
    }
    add
}

/// How an instance can read `VkPhysicalDeviceFeatures2`, which the subset's features need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Features2 {
    /// A 1.1+ instance: `vkGetPhysicalDeviceFeatures2`.
    Core,
    /// `VK_KHR_get_physical_device_properties2` was enabled: the `KHR` entry point.
    Khr,
    /// Neither: the subset is enabled with none of its optional features, the conservative reading.
    None,
}

impl Features2 {
    /// Which one an instance created with `api_version` and `extensions` has.
    pub(crate) fn of(api_version: u32, extensions: &[&CStr]) -> Features2 {
        if api_version >= vk::API_VERSION_1_1 {
            Features2::Core
        } else if extensions.contains(&PROPERTIES2) {
            Features2::Khr
        } else {
            Features2::None
        }
    }
}

/// What a device's portability subset is, when it has one. See this module's point 3.
#[derive(Clone, Copy)]
pub(crate) struct Subset {
    /// The supported features, read from the device -- or all false when the instance cannot read
    /// them ([`Features2::None`]), in which case none is enabled and every one is a gap.
    pub(crate) features: vk::PhysicalDevicePortabilitySubsetFeaturesKHR<'static>,
}

impl Subset {
    /// The subset of `physical`, or `None` when it exposes no `VK_KHR_portability_subset` (every
    /// native driver).
    ///
    /// # Safety
    ///
    /// `physical` must be a live device of `instance`, and `instance` must have been created from
    /// `entry` with the capability `features2` names.
    pub(crate) unsafe fn of(
        entry: &ash::Entry,
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        extensions: &[vk::ExtensionProperties],
        features2: Features2,
    ) -> Option<Subset> {
        if !extensions.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == SUBSET)) {
            return None;
        }
        let mut features = vk::PhysicalDevicePortabilitySubsetFeaturesKHR::default();
        {
            let mut query = vk::PhysicalDeviceFeatures2::default().push_next(&mut features);
            match features2 {
                // SAFETY: the caller's contract: live handles, and a 1.1 instance.
                Features2::Core => unsafe { instance.get_physical_device_features2(physical, &mut query) },
                Features2::Khr => {
                    let khr = ash::khr::get_physical_device_properties2::Instance::new(entry, instance);
                    // SAFETY: the caller's contract: the extension is enabled on this instance.
                    unsafe { khr.get_physical_device_features2(physical, &mut query) };
                }
                Features2::None => {}
            }
        }
        features.p_next = core::ptr::null_mut();
        Some(Subset { features })
    }

    /// The names of the subset features the device does **not** support (all of them when they
    /// could not be read), in the order the specification lists them.
    pub(crate) fn gaps(&self) -> Vec<&'static str> {
        let f = &self.features;
        [
            ("constantAlphaColorBlendFactors", f.constant_alpha_color_blend_factors),
            ("events", f.events),
            ("imageViewFormatReinterpretation", f.image_view_format_reinterpretation),
            ("imageViewFormatSwizzle", f.image_view_format_swizzle),
            ("imageView2DOn3DImage", f.image_view2_d_on3_d_image),
            ("multisampleArrayImage", f.multisample_array_image),
            ("mutableComparisonSamplers", f.mutable_comparison_samplers),
            ("pointPolygons", f.point_polygons),
            ("samplerMipLodBias", f.sampler_mip_lod_bias),
            ("separateStencilMaskRef", f.separate_stencil_mask_ref),
            ("shaderSampleRateInterpolationFunctions", f.shader_sample_rate_interpolation_functions),
            ("tessellationIsolines", f.tessellation_isolines),
            ("tessellationPointMode", f.tessellation_point_mode),
            ("triangleFans", f.triangle_fans),
            ("vertexAttributeAccessBeyondStride", f.vertex_attribute_access_beyond_stride),
        ]
        .into_iter()
        .filter(|&(_, supported)| supported == vk::FALSE)
        .map(|(name, _)| name)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN32: &CStr = c"VK_KHR_win32_surface";

    /// Only the loader's own "no non-portability driver" answer, only when the extension is on
    /// offer, and never twice.
    #[test]
    fn a_retry_needs_the_incompatible_driver_answer_and_the_extension_on_offer() {
        let offered = [WIN32, ENUMERATION];
        assert!(retry_with_portability(vk::Result::ERROR_INCOMPATIBLE_DRIVER, &offered, &[WIN32]));
        assert!(!retry_with_portability(vk::Result::ERROR_INITIALIZATION_FAILED, &offered, &[WIN32]));
        assert!(!retry_with_portability(vk::Result::ERROR_INCOMPATIBLE_DRIVER, &[WIN32], &[WIN32]));
        assert!(!retry_with_portability(vk::Result::ERROR_INCOMPATIBLE_DRIVER, &offered, &[ENUMERATION]));
    }

    #[test]
    fn properties2_is_added_only_below_1_1_and_only_when_offered_and_missing() {
        let offered = [ENUMERATION, PROPERTIES2];
        assert_eq!(portability_additions(&offered, &[], vk::API_VERSION_1_0), vec![ENUMERATION, PROPERTIES2]);
        assert_eq!(portability_additions(&offered, &[], vk::API_VERSION_1_1), vec![ENUMERATION]);
        assert_eq!(portability_additions(&[ENUMERATION], &[], vk::API_VERSION_1_0), vec![ENUMERATION]);
        assert_eq!(portability_additions(&offered, &[PROPERTIES2], vk::API_VERSION_1_0), vec![ENUMERATION]);
        assert_eq!(Features2::of(vk::API_VERSION_1_0, &[PROPERTIES2]), Features2::Khr);
        assert_eq!(Features2::of(vk::API_VERSION_1_2, &[]), Features2::Core);
        assert_eq!(Features2::of(vk::API_VERSION_1_0, &[]), Features2::None);
    }

    /// A gap is a feature that is false, by name; nothing else.
    #[test]
    fn the_gaps_are_the_false_features_by_name() {
        let mut features = vk::PhysicalDevicePortabilitySubsetFeaturesKHR::default()
            .constant_alpha_color_blend_factors(true)
            .events(true)
            .image_view_format_reinterpretation(true)
            .image_view_format_swizzle(true)
            .image_view2_d_on3_d_image(true)
            .multisample_array_image(true)
            .mutable_comparison_samplers(true)
            .separate_stencil_mask_ref(true)
            .shader_sample_rate_interpolation_functions(true)
            .triangle_fans(true)
            .vertex_attribute_access_beyond_stride(true);
        features.p_next = core::ptr::null_mut();
        let subset = Subset { features };
        assert_eq!(subset.gaps(), vec!["pointPolygons", "samplerMipLodBias", "tessellationIsolines", "tessellationPointMode"]);
        let none = Subset { features: vk::PhysicalDevicePortabilitySubsetFeaturesKHR::default() };
        assert_eq!(none.gaps().len(), 15);
    }
}
