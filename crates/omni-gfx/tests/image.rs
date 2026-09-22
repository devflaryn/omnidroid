//! Tests of the renderer's input type and its error surface. **No GPU, no display, no loader.**
//!
//! The frame type is where the guest's pixels enter Vulkan, and `Rgba8Image::new` is the only
//! thing between a guest-supplied length and a `memcpy` into mapped device memory — so the
//! assertions here are about exactness in *both* directions, not about a bound.

use omni_gfx::error::{GfxError, VkError};
use omni_gfx::{Rgba8Image, omni_texture};

#[test]
fn a_correctly_sized_buffer_is_accepted_and_reports_what_it_was_given() {
    let pixels = vec![0u8; 8 * 4 * 4];
    let image = Rgba8Image::new(8, 4, &pixels).unwrap();
    assert_eq!((image.width(), image.height()), (8, 4));
    assert_eq!(image.pixels().len(), 8 * 4 * 4);
    assert!(
        core::ptr::eq(image.pixels().as_ptr(), pixels.as_ptr()),
        "the image borrows the caller's buffer rather than copying it; a copy per frame is what \
         this type exists to avoid"
    );
}

#[test]
fn a_buffer_that_is_too_short_or_too_long_is_refused_with_both_numbers() {
    // Both directions, because they fail differently and only one of them looks like a bug: a
    // short buffer would be read past its end into mapped device memory, and a long one would
    // silently present a crop of what the caller meant while looking entirely correct.
    for provided in [16 * 16 * 4 - 1, 16 * 16 * 4 + 1, 0] {
        let pixels = vec![0u8; provided];
        let err = Rgba8Image::new(16, 16, &pixels).unwrap_err();
        assert_eq!(
            err,
            GfxError::ImageLengthMismatch {
                width: 16,
                height: 16,
                expected: 16 * 16 * 4,
                provided,
            },
            "a {provided}-byte buffer is not a 16x16 RGBA8 image"
        );
        // Global Constraint 7: the message names the values that were wrong.
        let text = err.to_string();
        assert!(text.contains(&provided.to_string()), "{text}");
        assert!(text.contains(&(16 * 16 * 4).to_string()), "{text}");
    }
}

#[test]
fn a_zero_dimension_is_refused_as_an_extent_and_not_as_a_length() {
    // The distinction is real: a `0 x 0` image with an empty slice is perfectly
    // length-consistent, so a check written only against the byte count would accept it — and
    // `VkImageBlit`'s source region must have a non-zero extent, so the failure would surface
    // inside the driver.
    let empty: [u8; 0] = [];
    for (width, height) in [(0, 16), (16, 0), (0, 0)] {
        let err = Rgba8Image::new(width, height, &empty).unwrap_err();
        assert_eq!(
            err,
            GfxError::ZeroExtentImage { width, height },
            "{width}x{height} must be refused as an extent"
        );
    }
}

#[test]
fn the_length_this_type_requires_is_the_one_omni_texture_computes() {
    // The seam between D27's transcoder and this renderer, asserted rather than assumed. If the
    // two ever disagreed, every decoded texture would be refused by `Rgba8Image::new` with a
    // number that looked right.
    for (width, height) in [(4u32, 4u32), (16, 16), (1, 1), (37, 61), (640, 480)] {
        let expected = omni_texture::decoded_len(width, height).unwrap();
        let pixels = vec![0u8; expected];
        let image = Rgba8Image::new(width, height, &pixels)
            .unwrap_or_else(|err| panic!("{width}x{height}: {err}"));
        assert_eq!(image.pixels().len(), expected);
    }
}

#[test]
fn a_decoded_etc1_block_is_directly_presentable() {
    // D27's whole path in three lines: the APK ships `GL_ETC1_RGB8_OES` and nothing else
    // compressed (`tools/texture_census.py`), this host's GPU samples neither ETC2 nor ASTC
    // (`docs/research/graphics-spike.md` §3), so the transcoder decodes to RGBA8 — and RGBA8 is
    // what the renderer presents. This asserts the two halves actually fit, with a real decode
    // rather than a buffer of the right size.
    let block = [0u8; 8];
    let mut decoded = vec![0u8; omni_texture::decoded_len(4, 4).unwrap()];
    omni_texture::decode(omni_texture::CompressedFormat::Etc1Rgb8, &block, 4, 4, &mut decoded)
        .expect("an all-zero ETC1 block is a valid individual-mode block");
    let image = Rgba8Image::new(4, 4, &decoded).expect("a decoded block is a presentable frame");
    assert_eq!(image.pixels().len(), 64);
    // ETC1 has no alpha channel and the decoder sets it to 255, which is what a sampler must see
    // for a `GL_RGB` base internal format. A frame with a zero alpha channel would present as
    // fully transparent on a surface with a non-opaque composite alpha.
    for (index, texel) in image.pixels().chunks_exact(4).enumerate() {
        assert_eq!(texel[3], 255, "texel {index} has alpha {}", texel[3]);
    }
}

#[test]
fn a_vulkan_result_renders_with_its_symbolic_name() {
    // A bare number in a log is not diagnostic, and this crate's failures are the *only* record
    // of what went wrong: this host has no validation layers (spike §6), so nothing else is
    // writing anything down.
    for (code, name) in [
        (-4, "VK_ERROR_DEVICE_LOST"),
        (-1_000_001_004, "VK_ERROR_OUT_OF_DATE_KHR"),
        (-1_000_000_000, "VK_ERROR_SURFACE_LOST_KHR"),
        (-2, "VK_ERROR_OUT_OF_DEVICE_MEMORY"),
        (1_000_001_003, "VK_SUBOPTIMAL_KHR"),
    ] {
        let err = VkError(code);
        assert_eq!(err.name(), Some(name));
        let text = err.to_string();
        assert!(text.contains(name), "{text}");
        assert!(text.contains(&code.to_string()), "{text}");
    }
    // A code with no name still prints the number, because the number is what can be looked up.
    let unknown = VkError(-1_234_567);
    assert_eq!(unknown.name(), None);
    assert!(unknown.to_string().contains("-1234567"), "{unknown}");
}

#[test]
fn device_lost_is_distinguishable_from_every_other_failure() {
    // It is the one failure where retrying is guaranteed to fail again and every object in the
    // renderer is already invalid, so a caller has to be able to tell it apart without matching
    // two levels of enum.
    let lost = GfxError::Vulkan {
        operation: "present",
        api: "vkQueueSubmit",
        result: VkError(-4),
    };
    assert!(lost.is_device_lost());
    assert!(VkError(-4).is_device_lost());

    for other in [
        GfxError::Vulkan { operation: "present", api: "x", result: VkError(-2) },
        GfxError::LoaderMissing { detail: "no vulkan-1.dll".to_owned() },
        GfxError::SurfaceCannotBeTransferDestination,
        GfxError::ZeroExtentImage { width: 0, height: 0 },
        GfxError::UnsupportedWindowSystem { system: "wayland" },
    ] {
        assert!(!other.is_device_lost(), "{other} is not a lost device");
    }
}

#[test]
fn every_failure_names_what_it_was_doing_or_what_it_could_not_find() {
    // Global Constraint 7, asserted variant by variant rather than by counting them: a new
    // variant with a message that says only "failed" would not change any total.
    let cases: [(GfxError, &str); 8] = [
        (GfxError::LoaderMissing { detail: "ERROR_MOD_NOT_FOUND".to_owned() }, "ERROR_MOD_NOT_FOUND"),
        (
            GfxError::Vulkan { operation: "present", api: "vkQueuePresentKHR", result: VkError(-4) },
            "vkQueuePresentKHR",
        ),
        (
            GfxError::MissingInstanceExtension { name: "VK_KHR_win32_surface", why: "no driver" },
            "VK_KHR_win32_surface",
        ),
        (
            GfxError::NoUsableDevice { considered: 2, detail: "lavapipe: no swapchain".to_owned() },
            "lavapipe: no swapchain",
        ),
        (
            GfxError::NoUsableSurfaceFormat { device: "RTX 4060".to_owned(), offered: 7 },
            "RTX 4060",
        ),
        (GfxError::SurfaceCannotBeTransferDestination, "TRANSFER_DST"),
        (
            GfxError::NoUsableMemoryType { required: "DEVICE_LOCAL", type_bits: 0b1010 },
            "DEVICE_LOCAL",
        ),
        (GfxError::UnsupportedWindowSystem { system: "wayland" }, "wayland"),
    ];
    for (err, expected) in cases {
        let text = err.to_string();
        assert!(text.contains(expected), "{err:?} does not name {expected}: {text}");
    }
}
