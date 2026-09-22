//! Tests of every choice the renderer makes, against tables it will never see on this machine.
//!
//! **These need no GPU, no display and no Vulkan loader**, which is the whole reason
//! `omni_gfx::select` exists as a separate module: a renderer's real defects are usually choices,
//! and a choice tested only against the one driver it was written on is a choice nobody has
//! checked. The live tests are in `renderer_live.rs`, gated and `#[ignore]`d.
//!
//! Two kinds of fixture appear below and they are labelled:
//!
//! * **Measured.** Tables transcribed from `docs/research/graphics-spike.md`, which were produced
//!   by code run on this host (RTX 4060, driver 591.86, Vulkan 1.4.325). These assert that the
//!   renderer does the right thing on the machine it will actually run on.
//! * **Synthetic.** Tables no machine here has — an integrated GPU where every memory type is
//!   host-visible, a surface that offers no `SRGB_NONLINEAR` format, a driver with no linear blit
//!   filter. These are the ones a single-machine test suite cannot otherwise reach.

use ash::vk;
use omni_gfx::select::{
    self, PresentMode, blit_filter, clamp_extent, device_rank, image_count, memory_type,
    present_mode, surface_format,
};

/// **Measured.** This host's five memory types, `docs/research/graphics-spike.md` §3.
///
/// | Type | Heap | Flags |
/// |---|---|---|
/// | 0 | 1 | (none) |
/// | 1 | 0 | `DEVICE_LOCAL` |
/// | 2 | 1 | `HOST_VISIBLE \| HOST_COHERENT` |
/// | 3 | 1 | `HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED` |
/// | 4 | 2 | `DEVICE_LOCAL \| HOST_VISIBLE \| HOST_COHERENT` (a ~214 MiB ReBAR window) |
fn measured_memory_properties() -> vk::PhysicalDeviceMemoryProperties {
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    memory_properties(&[
        (vk::MemoryPropertyFlags::empty(), 1),
        (vk::MemoryPropertyFlags::DEVICE_LOCAL, 0),
        (host, 1),
        (host | vk::MemoryPropertyFlags::HOST_CACHED, 1),
        (vk::MemoryPropertyFlags::DEVICE_LOCAL | host, 2),
    ])
}

fn memory_properties(
    types: &[(vk::MemoryPropertyFlags, u32)],
) -> vk::PhysicalDeviceMemoryProperties {
    let mut props = vk::PhysicalDeviceMemoryProperties {
        memory_type_count: types.len() as u32,
        ..Default::default()
    };
    for (slot, (flags, heap)) in props.memory_types.iter_mut().zip(types) {
        slot.property_flags = *flags;
        slot.heap_index = *heap;
    }
    props
}

fn caps(min: u32, max: u32, current: Option<(u32, u32)>) -> vk::SurfaceCapabilitiesKHR {
    vk::SurfaceCapabilitiesKHR {
        min_image_count: min,
        max_image_count: max,
        // `0xFFFFFFFF` in both axes is Vulkan's "you choose".
        current_extent: current.map_or(
            vk::Extent2D { width: u32::MAX, height: u32::MAX },
            |(width, height)| vk::Extent2D { width, height },
        ),
        min_image_extent: vk::Extent2D { width: 16, height: 16 },
        max_image_extent: vk::Extent2D { width: 4096, height: 4096 },
        ..Default::default()
    }
}

fn format(format: vk::Format, color_space: vk::ColorSpaceKHR) -> vk::SurfaceFormatKHR {
    vk::SurfaceFormatKHR { format, color_space }
}

// ---------------------------------------------------------------------------------------------
// Present mode
// ---------------------------------------------------------------------------------------------

#[test]
fn a_present_mode_this_surface_offers_is_the_one_used() {
    // **Measured.** All four of these were offered on this host and all four were *granted*, with
    // none silently falling back (spike §4).
    let offered = [
        vk::PresentModeKHR::FIFO,
        vk::PresentModeKHR::FIFO_RELAXED,
        vk::PresentModeKHR::MAILBOX,
        vk::PresentModeKHR::IMMEDIATE,
    ];
    // Asserted over every variant rather than over one, so that a new `PresentMode` added without
    // a `to_vk` arm cannot slip through (VERIFICATION entry 1: membership, not a total).
    for mode in PresentMode::ALL {
        assert_eq!(
            present_mode(mode, &offered),
            mode.to_vk(),
            "{mode:?} is offered by this surface and must not be downgraded"
        );
    }
}

#[test]
fn an_unavailable_present_mode_degrades_to_fifo_and_never_to_a_tearing_one() {
    // **Synthetic.** A surface offering only what the specification guarantees.
    let minimal = [vk::PresentModeKHR::FIFO];
    for mode in PresentMode::ALL {
        assert_eq!(
            present_mode(mode, &minimal),
            vk::PresentModeKHR::FIFO,
            "{mode:?} must degrade to FIFO, the only mode every implementation must offer"
        );
    }

    // And the specific downgrade that would be a visible regression: MAILBOX is tear-free, so
    // falling back to IMMEDIATE — which is *present* here — would swap a tear-free mode for a
    // tearing one without anybody asking.
    let no_mailbox = [vk::PresentModeKHR::FIFO, vk::PresentModeKHR::IMMEDIATE];
    assert_eq!(
        present_mode(PresentMode::Mailbox, &no_mailbox),
        vk::PresentModeKHR::FIFO,
        "MAILBOX must not fall back to IMMEDIATE just because it is available"
    );
}

#[test]
fn fifo_is_the_default_present_mode() {
    // Not a tautology: `PresentMode` derives `Default`, and a derive puts it on the *first*
    // variant. This asserts the ordering of the enum is the intended default rather than an
    // accident of how it was written down.
    assert_eq!(PresentMode::default(), PresentMode::Fifo);
    assert_eq!(select::PresentMode::default().to_vk(), vk::PresentModeKHR::FIFO);
}

// ---------------------------------------------------------------------------------------------
// Image count
// ---------------------------------------------------------------------------------------------

#[test]
fn the_image_count_is_one_more_than_the_minimum_this_surface_requires() {
    // **Measured.** This host reports min/max = 2/8 (spike §4), so the renderer asks for 3: one
    // to render into while the presentation engine holds one.
    assert_eq!(image_count(&caps(2, 8, None)), 3);
}

#[test]
fn a_maximum_image_count_of_zero_means_unlimited_rather_than_none() {
    // **Synthetic**, and the trap in this call: `max_image_count == 0` is Vulkan's "no limit". An
    // implementation that clamped to it literally would create a swapchain with **no images**,
    // and `vkAcquireNextImageKHR` would then never return an index.
    assert_eq!(image_count(&caps(2, 0, None)), 3, "0 is 'no limit', not 'zero images'");
    assert_eq!(image_count(&caps(5, 0, None)), 6);
}

#[test]
fn the_image_count_is_clamped_when_the_surface_caps_it() {
    // **Synthetic.** A surface that allows exactly its own minimum, which is legal and which a
    // `min + 1` with no clamp would exceed.
    assert_eq!(image_count(&caps(2, 2, None)), 2);
    assert_eq!(image_count(&caps(3, 4, None)), 4);
}

// ---------------------------------------------------------------------------------------------
// Extent
// ---------------------------------------------------------------------------------------------

#[test]
fn the_surfaces_own_extent_wins_over_whatever_the_caller_wanted() {
    // On Win32 `currentExtent` *is* the window's client area, so a swapchain built at any other
    // size is out of date the moment it is created. The desired size is deliberately absurd here
    // to prove it is not consulted at all.
    let caps = caps(2, 8, Some((1024, 768)));
    assert_eq!(clamp_extent((99, 99), &caps), vk::Extent2D { width: 1024, height: 768 });
    assert_eq!(clamp_extent((0, 0), &caps), vk::Extent2D { width: 1024, height: 768 });
}

#[test]
fn the_desired_extent_is_used_and_clamped_only_when_the_surface_declines_to_state_one() {
    // **Synthetic.** `0xFFFFFFFF` is what some Wayland compositors report and what no Win32
    // driver does, so this arm cannot be reached on this machine at all.
    let caps = caps(2, 8, None);
    assert_eq!(clamp_extent((800, 600), &caps), vk::Extent2D { width: 800, height: 600 });
    assert_eq!(
        clamp_extent((1, 1), &caps),
        vk::Extent2D { width: 16, height: 16 },
        "below the surface's minimum, so clamped up"
    );
    assert_eq!(
        clamp_extent((99_999, 99_999), &caps),
        vk::Extent2D { width: 4096, height: 4096 },
        "above the surface's maximum, so clamped down"
    );
}

#[test]
fn a_minimised_window_stays_zero_rather_than_being_clamped_up_to_the_minimum() {
    // The load-bearing case. A minimised window reports `(0, 0)`, and clamping that up to the
    // surface's 16x16 minimum would produce a swapchain for a window with no pixels — which
    // would present nothing, forever, with no error anywhere.
    let caps = caps(2, 8, None);
    for desired in [(0, 600), (800, 0), (0, 0)] {
        assert_eq!(
            clamp_extent(desired, &caps),
            vk::Extent2D { width: 0, height: 0 },
            "{desired:?} is a minimised window and must stay zero"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Surface format
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unorm_format_is_preferred_over_the_srgb_one_this_host_offers_first() {
    // **Measured-ish.** This host offers 7 formats including `B8G8R8A8_SRGB`/`SRGB_NONLINEAR`
    // plus HDR10 and extended-linear variants (spike §4). The renderer blits already-encoded
    // guest pixels straight into the swapchain image, so an `_SRGB` swapchain would encode them
    // a second time.
    let offered = [
        format(vk::Format::B8G8R8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        format(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        format(vk::Format::R8G8B8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        format(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT),
    ];
    let chosen = surface_format(&offered).expect("this list has a usable format");
    assert_eq!(chosen.format, vk::Format::B8G8R8A8_UNORM);
    assert_eq!(chosen.color_space, vk::ColorSpaceKHR::SRGB_NONLINEAR);
}

#[test]
fn a_non_srgb_nonlinear_colour_space_is_never_chosen_even_when_its_format_is_preferred() {
    // **Synthetic**, and the sharp edge: the *format* here is exactly the preferred one, and the
    // colour space is not. A search that matched on format alone would take it and every pixel
    // the renderer wrote would mean something different from what it meant.
    let offered = [
        format(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT),
        format(vk::Format::R8G8B8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR),
    ];
    let chosen = surface_format(&offered).expect("the second entry is usable");
    assert_eq!(chosen.format, vk::Format::R8G8B8A8_UNORM);
    assert_eq!(chosen.color_space, vk::ColorSpaceKHR::SRGB_NONLINEAR);
}

#[test]
fn an_unrecognised_format_is_taken_rather_than_refused_when_nothing_preferred_is_offered() {
    // **Synthetic.** A driver offering only a packed format none of the preferences name. The
    // colour may be wrong; a black window certainly is.
    let offered = [format(vk::Format::R5G6B5_UNORM_PACK16, vk::ColorSpaceKHR::SRGB_NONLINEAR)];
    assert_eq!(surface_format(&offered).map(|f| f.format), Some(vk::Format::R5G6B5_UNORM_PACK16));
}

#[test]
fn a_surface_with_no_usable_colour_space_at_all_is_refused_rather_than_guessed_at() {
    let offered = [
        format(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT),
        format(vk::Format::R16G16B16A16_SFLOAT, vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT),
    ];
    assert_eq!(surface_format(&offered), None);
    assert_eq!(surface_format(&[]), None, "an empty candidate list is a refusal, not a panic");
}

// ---------------------------------------------------------------------------------------------
// Memory types
// ---------------------------------------------------------------------------------------------

#[test]
fn staging_and_device_local_allocations_land_on_different_memory_types_on_this_host() {
    // **Measured**, and the reason this function has a test. This host has a
    // `DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT` type — a ReBAR window — which satisfies
    // *both* searches and is only ~214 MiB against a 7.77 GiB device-local heap (spike §3). An
    // implementation that landed both allocations there would work perfectly until a frame did
    // not fit.
    let props = measured_memory_properties();
    let all = u32::MAX;
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

    let staging = memory_type(&props, all, host).expect("this host has host-visible memory");
    let device_local = memory_type(&props, all, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .expect("this host has device-local memory");
    assert_eq!(staging, 2, "type 2 is the first plain host-visible coherent type");
    assert_eq!(device_local, 1, "type 1 is the first device-local type, on the 7.77 GiB heap");
    assert_ne!(
        staging, device_local,
        "the staging buffer and the staging image must not both land in the 214 MiB ReBAR window"
    );
    assert_eq!(props.memory_types[staging as usize].heap_index, 1, "system RAM");
    assert_eq!(props.memory_types[device_local as usize].heap_index, 0, "the 7.77 GiB VRAM heap");
}

#[test]
fn the_resources_own_type_bits_are_respected_and_not_merely_the_flags() {
    // **Measured table, synthetic mask.** With types 2 and 3 forbidden by the resource, the only
    // host-visible coherent type left is 4 — the ReBAR window — and the search must find it
    // rather than returning a type the resource cannot use.
    let props = measured_memory_properties();
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let mask = 0b1_0011;
    assert_eq!(memory_type(&props, mask, host), Some(4));
    assert_eq!(memory_type(&props, 0b0_0010, host), None, "type 1 is not host visible");
    assert_eq!(memory_type(&props, 0, host), None, "a resource that permits nothing gets nothing");
}

#[test]
fn an_integrated_gpu_where_everything_is_host_visible_still_resolves() {
    // **Synthetic**, and unreachable on this machine: a GPU whose single heap is shared with the
    // CPU, so both searches legitimately land on the same type. The point is that they *resolve*
    // — an implementation that refused when the two searches collided would not start at all on
    // a laptop.
    let both = vk::MemoryPropertyFlags::DEVICE_LOCAL
        | vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT;
    let props = memory_properties(&[(both, 0)]);
    assert_eq!(memory_type(&props, u32::MAX, vk::MemoryPropertyFlags::DEVICE_LOCAL), Some(0));
    assert_eq!(
        memory_type(
            &props,
            u32::MAX,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        ),
        Some(0)
    );
}

#[test]
fn a_device_with_no_host_visible_memory_is_reported_rather_than_worked_around() {
    // **Synthetic.** No such device exists in practice, which is exactly why the arm needs a test
    // rather than a reviewer's confidence: `GfxError::NoUsableMemoryType` is what this `None`
    // becomes, and it names the requirement and the mask.
    let props = memory_properties(&[(vk::MemoryPropertyFlags::DEVICE_LOCAL, 0)]);
    assert_eq!(
        memory_type(&props, u32::MAX, vk::MemoryPropertyFlags::HOST_VISIBLE),
        None,
        "nothing satisfies the requirement, so the answer is None and not type 0"
    );
}

// ---------------------------------------------------------------------------------------------
// Device ranking and blit filter
// ---------------------------------------------------------------------------------------------

#[test]
fn a_real_gpu_outranks_every_software_or_virtual_one() {
    // D8's constraint: `libroblox.so` refuses emulated Vulkan devices by string match, so the
    // device this picks must be a genuine one. Asserted as a strict ordering over the whole enum
    // rather than as "discrete is highest", because the failure that matters is a *tie* — two
    // types ranking equally means `devices[0]` decides, which on a machine with lavapipe
    // installed is a coin toss.
    let order = [
        vk::PhysicalDeviceType::DISCRETE_GPU,
        vk::PhysicalDeviceType::INTEGRATED_GPU,
        vk::PhysicalDeviceType::VIRTUAL_GPU,
        vk::PhysicalDeviceType::OTHER,
        vk::PhysicalDeviceType::CPU,
    ];
    for pair in order.windows(2) {
        assert!(
            device_rank(pair[0]) > device_rank(pair[1]),
            "{:?} must strictly outrank {:?}",
            pair[0],
            pair[1]
        );
    }
    assert_eq!(
        device_rank(vk::PhysicalDeviceType::CPU),
        0,
        "a software rasteriser is the last resort, not merely a low-ranked one"
    );
}

#[test]
fn the_blit_filter_is_linear_only_when_the_source_format_actually_supports_it() {
    // Both arms, because only one of them is reachable on any conformant driver:
    // `R8G8B8A8_UNORM` is *required* to support linear filtering, so the `NEAREST` arm exists for
    // a driver nobody here has — and an invalid filter is undefined behaviour that, on a host
    // with no validation layers, nothing would report.
    let linear = vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        | vk::FormatFeatureFlags::BLIT_SRC
        | vk::FormatFeatureFlags::BLIT_DST;
    assert_eq!(blit_filter(linear), vk::Filter::LINEAR);
    assert_eq!(blit_filter(vk::FormatFeatureFlags::BLIT_SRC), vk::Filter::NEAREST);
    assert_eq!(blit_filter(vk::FormatFeatureFlags::empty()), vk::Filter::NEAREST);
}
