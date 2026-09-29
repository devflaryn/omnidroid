//! Which of the host's GPU APIs the device's graphics stand on: **Vulkan** (the D3a design: the
//! guest's Vulkan driver forwards to the host's Vulkan, and GLES is the image's ANGLE on it) or
//! **GL**, the fallback for a host whose GPU has no Vulkan driver: the guest's GLES driver
//! (`libGLES_omni.so`, [`super::gl`]) forwards every GLES command to the host's GLES, and the device
//! has no Vulkan at all.
//!
//! `OMNI_GPU=vulkan|gl|auto` (default `auto`). `auto` takes Vulkan when the host has a Vulkan
//! device that is a GPU, and GL when it has none (a CPU rasteriser such as lavapipe is not one: the
//! engine refuses it as emulated, D8) but has a GLES it can open; else Vulkan. The choice is made
//! once, by the first host process of an instance, and written back to `OMNI_GPU` so every host
//! process it starts inherits the same device.
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Gl,
}

impl Backend {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vulkan => "vulkan",
            Self::Gl => "gl",
        }
    }
}

/// What `OMNI_GPU` asks for, if it names a backend.
#[must_use]
pub fn asked(value: Option<&str>) -> Option<Backend> {
    match value.map(str::trim) {
        Some("vulkan") => Some(Backend::Vulkan),
        Some("gl" | "gles" | "opengl") => Some(Backend::Gl),
        _ => None,
    }
}

/// `auto`'s rule, given what the host has.
#[must_use]
pub fn choose(host_has_vulkan_gpu: bool, host_has_gles: bool) -> Backend {
    if host_has_vulkan_gpu || !host_has_gles {
        Backend::Vulkan
    } else {
        Backend::Gl
    }
}

/// This instance's backend (see the module documentation).
pub fn backend() -> Backend {
    static CHOSEN: OnceLock<Backend> = OnceLock::new();
    *CHOSEN.get_or_init(|| {
        let asked_for = asked(std::env::var("OMNI_GPU").ok().as_deref());
        let chosen = asked_for.unwrap_or_else(|| {
            let vulkan = host_vulkan_gpu();
            let gles = super::gl::host_available();
            let chosen = choose(vulkan.is_some(), gles.is_ok());
            eprintln!(
                "[gpu] OMNI_GPU=auto: host Vulkan GPU {}; host GLES {} -> {}",
                vulkan.as_deref().unwrap_or("none"),
                gles.as_deref().map_or_else(|e| format!("none ({e})"), |d| d.to_string()),
                chosen.name()
            );
            chosen
        });
        // Every host process this one starts sees the same device.
        std::env::set_var("OMNI_GPU", chosen.name());
        chosen
    })
}

/// The name of the host's first Vulkan device that is not a CPU rasteriser, if it has one.
fn host_vulkan_gpu() -> Option<String> {
    use ash::vk;
    let entry = super::entry().ok()?;
    let app = vk::ApplicationInfo { api_version: vk::API_VERSION_1_1, ..Default::default() };
    let info = vk::InstanceCreateInfo { p_application_info: &app, ..Default::default() };
    // SAFETY: a minimal instance, destroyed below; the host loader is loaded (`entry`).
    let instance = unsafe { entry.create_instance(&info, None) }.ok()?;
    // SAFETY: `instance` is live until destroyed at the end of this function.
    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    let found = devices.into_iter().find_map(|d| {
        // SAFETY: `d` is one of `instance`'s physical devices.
        let props = unsafe { instance.get_physical_device_properties(d) };
        (props.device_type != vk::PhysicalDeviceType::CPU)
            .then(|| props.device_name_as_c_str().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    });
    // SAFETY: nothing made from `instance` outlives it.
    unsafe { instance.destroy_instance(None) };
    found
}

/// The device's properties for `backend`: which EGL driver Android's loader takes, and whether
/// there is a Vulkan driver at all (`hw_get_module("vulkan")` finds none when the name is empty:
/// no `vulkan.<variant>.so` of this image matches the device's other variant names).
#[must_use]
pub fn properties(backend: Backend) -> [(&'static str, &'static str); 2] {
    match backend {
        Backend::Vulkan => [("ro.hardware.egl", "angle"), ("ro.hardware.vulkan", "omni")],
        Backend::Gl => [("ro.hardware.egl", "omni"), ("ro.hardware.vulkan", "")],
    }
}

/// The image's files a device on `backend` does not have: with no Vulkan driver, the features that
/// say it has Vulkan (an app that believes them tries a Vulkan device that is not there).
#[must_use]
pub fn left_out(backend: Backend) -> &'static [&'static str] {
    match backend {
        Backend::Vulkan => &[],
        Backend::Gl => &[
            "/vendor/etc/permissions/android.hardware.vulkan.compute.xml",
            "/vendor/etc/permissions/android.hardware.vulkan.level.xml",
            "/vendor/etc/permissions/android.hardware.vulkan.version.xml",
            "/vendor/etc/permissions/android.software.vulkan.deqp.level.xml",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omni_gpu_names_a_backend_or_leaves_it_to_auto() {
        assert_eq!(asked(Some("gl")), Some(Backend::Gl));
        assert_eq!(asked(Some("vulkan")), Some(Backend::Vulkan));
        assert_eq!(asked(Some("auto")), None);
        assert_eq!(asked(None), None);
    }

    #[test]
    fn auto_takes_gl_only_when_the_host_has_no_vulkan_gpu_and_has_gles() {
        assert_eq!(choose(true, true), Backend::Vulkan);
        assert_eq!(choose(true, false), Backend::Vulkan);
        assert_eq!(choose(false, true), Backend::Gl);
        // Neither: Vulkan (a CPU rasteriser, if any) rather than a GL that cannot open.
        assert_eq!(choose(false, false), Backend::Vulkan);
    }

    #[test]
    fn the_gl_device_has_the_omni_egl_driver_and_no_vulkan() {
        assert_eq!(properties(Backend::Gl), [("ro.hardware.egl", "omni"), ("ro.hardware.vulkan", "")]);
        assert_eq!(properties(Backend::Vulkan), [("ro.hardware.egl", "angle"), ("ro.hardware.vulkan", "omni")]);
        assert!(left_out(Backend::Gl).iter().all(|f| f.contains("vulkan")));
        assert!(left_out(Backend::Vulkan).is_empty());
    }
}
