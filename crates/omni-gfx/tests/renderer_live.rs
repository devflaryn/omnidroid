//! Tests that open a real window, load a real Vulkan driver and put real frames on a real GPU.
//!
//! # The gate, and why it is a failure rather than a skip
//!
//! VERIFICATION entry 4: a test that early-returns when its fixture is missing reports `ok` and
//! proves nothing — which is how two of the filesystem seam's confinement rules came to have never
//! executed on the machine whose green suite was the evidence for them. A GPU and a desktop
//! session are exactly that kind of fixture, and a build machine may have neither. So the two
//! states are made visibly different instead of reconciled:
//!
//! * **Not asked for.** Everything here is `#[ignore]`d, so an ordinary `cargo test --workspace
//!   --release` prints a line saying it did not run.
//! * **Asked for.** `--ignored` means somebody decided these should run, and if
//!   `OMNI_GFX_WINDOW_TESTS=1` is not set they **panic** naming the variable. Nothing here is
//!   allowed to quietly pass on a machine that cannot run it.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-gfx --release --test renderer_live -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is a recommendation rather than a requirement: each test owns its own
//! window, device and swapchain, and four concurrent instances were measured to cost ~52 MiB of
//! VRAM each and to scale in aggregate (`docs/research/graphics-spike.md` §5). Serialising them
//! only makes a failure easier to read.
//!
//! # What these can and cannot see
//!
//! They can see that frames are submitted, that the swapchain follows the window, and that the
//! renderer's own counters agree with the outcomes it returned. They **cannot see what is on the
//! screen**: the spike measured that `PrintWindow(PW_RENDERFULLCONTENT)` captures the title bar
//! and returns solid black for the client area of a flip-model swapchain on this host (§1), so
//! there is no automated read-back of a presented frame, and the "the triangle renders" class of
//! claim is not one this file makes. That limitation is the spike's, not this file's, and it is
//! recorded rather than worked around.

use std::time::{Duration, Instant};

use ash::vk;
use omni_gfx::vulkan::{FrameOutcome, Renderer, RendererConfig};
use omni_gfx::{PresentMode, Rgba8Image};
use omni_platform::window::{Window, WindowDesc, WindowEvent};

/// The opt-in, shared with `omni-platform`'s `window_live.rs`: one decision by one person covers
/// the whole graphics bring-up.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Fail, naming the variable, if these were run without the opt-in. See the module header.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens a window, loads \
         the Vulkan driver and presents frames on the GPU; it will not pretend to have passed on \
         a machine that cannot do that. Set {GATE}=1 to run it, or drop --ignored to skip it \
         visibly."
    );
}

/// A window and a renderer for it, sized `width` x `height`.
fn harness(title: &str, width: u32, height: u32) -> (Window, Renderer) {
    let mut window = Window::new(&WindowDesc::new(title, width, height))
        .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    // Drain creation's own resize burst so each test starts from a known queue.
    let _ = window.poll_events().count();
    let size = window.client_size().unwrap();
    let renderer = Renderer::new(window.raw(), size, RendererConfig::default())
        .unwrap_or_else(|err| panic!("could not create the renderer: {err}"));
    (window, renderer)
}

/// Pump the window and feed every resize to the renderer. Returns whether a close was requested,
/// which nothing here acts on but which would otherwise be silently dropped.
fn pump(window: &mut Window, renderer: &mut Renderer) {
    for event in window.poll_events() {
        if let WindowEvent::Resized { width, height } = event {
            renderer.notify_resized(width, height);
        }
    }
}

/// Present frames until `done` is satisfied or the deadline passes, pumping the window each time.
///
/// A bounded poll on the thing under test rather than a sleep (VERIFICATION entry 6). The deadline
/// bounds only failure; a passing run leaves as soon as the condition holds.
///
/// Returns how many of each outcome were seen, because the relation between those counts and the
/// renderer's own counters is what several of these tests actually assert.
fn drive(
    window: &mut Window,
    renderer: &mut Renderer,
    what: &str,
    mut done: impl FnMut(&Renderer) -> bool,
) -> Outcomes {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcomes = Outcomes::default();
    loop {
        pump(window, renderer);
        match renderer.present_clear([0.02, 0.02, 0.08, 1.0]) {
            Ok(FrameOutcome::Presented) => outcomes.presented += 1,
            Ok(FrameOutcome::SwapchainRecreated) => outcomes.recreated += 1,
            Ok(FrameOutcome::Skipped) => outcomes.skipped += 1,
            Err(err) => panic!("presenting while waiting for {what} failed: {err}"),
        }
        if done(renderer) {
            return outcomes;
        }
        assert!(
            Instant::now() < deadline,
            "waited 10s for {what}; the renderer is {renderer:?} and the outcomes were {outcomes:?}"
        );
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Outcomes {
    presented: u64,
    recreated: u64,
    skipped: u64,
}

impl Outcomes {
    fn total(self) -> u64 {
        self.presented + self.recreated + self.skipped
    }
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_renderer_picks_a_genuine_device_and_reports_what_it_picked() {
    require_gate();
    let (_window, renderer) = harness("omnidroid: device", 640, 480);
    let report = renderer.device();

    assert!(!report.name.is_empty(), "the device must report a name");
    // D8's constraint, asserted rather than assumed: `libroblox.so` refuses emulated Vulkan
    // devices by string match, so a software rasteriser here would be a renderer that works and
    // that Roblox would then decline to use.
    assert_ne!(
        report.device_type,
        vk::PhysicalDeviceType::CPU,
        "{} is a software rasteriser; D8 records that Roblox refuses emulated devices",
        report.name
    );
    // Both families exist on the device and both were used to create it. On this host they are
    // the same family (spike §3: family 0 does graphics, compute and transfer), which is what
    // lets the swapchain be EXCLUSIVE — but the assertion is the weaker, portable one.
    //
    // The layers are the **loader's**, exactly: read here independently, from the same loader the
    // renderer found (the platform's candidates in order, or `Entry::load()` where it names none).
    // Not "non-empty": that was a fact about the Windows host, whose loader carries five implicit
    // layers (spike §6); this project's macOS host has none installed (MEASURED, `vulkaninfo`), and
    // an empty list there is the truth.
    let candidates = omni_platform::window::vulkan_loader_candidates();
    // SAFETY: loading the host's Vulkan loader, as the renderer did.
    let entry = if candidates.is_empty() {
        unsafe { ash::Entry::load() }.ok()
    } else {
        candidates.iter().find_map(|path| unsafe { ash::Entry::load_from(path) }.ok())
    }
    .expect("the loader the renderer loaded");
    // SAFETY: takes no handles.
    let layers: Vec<String> = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap()
        .iter()
        .map(|layer| layer.layer_name_as_c_str().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(report.available_layers, layers, "the report lists the loader's layers");
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn validation_is_enabled_exactly_when_the_host_has_the_layer() {
    require_gate();
    let (_window, renderer) = harness("omnidroid: validation", 320, 240);
    let report = renderer.device();

    // The **relation**, not the host fact. `docs/research/graphics-spike.md` §6 measured that
    // this machine has five layers and that `VK_LAYER_KHRONOS_validation` is not among them — but
    // that document's own first recommendation is to install it, and a test that pinned "false"
    // would fail the day somebody followed the advice. What must hold either way is that the
    // renderer enables the layer when it is there and starts regardless when it is not.
    let present =
        report.available_layers.iter().any(|name| name == "VK_LAYER_KHRONOS_validation");
    assert_eq!(
        renderer.validation_enabled(),
        present,
        "validation must be enabled exactly when available; layers were {:?}",
        report.available_layers
    );
    // And the absence must be non-fatal, which is what getting this far proves: the renderer
    // exists, with a swapchain.
    assert!(renderer.swapchain_extent().is_some());
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn the_swapchain_matches_the_window_and_uses_an_unencoded_format() {
    require_gate();
    let (window, renderer) = harness("omnidroid: swapchain", 800, 600);

    assert_eq!(
        renderer.swapchain_extent(),
        Some(window.client_size().unwrap()),
        "the swapchain must be the size of the window's client area, not of the outer window"
    );
    assert_eq!(renderer.swapchain_generations(), 1, "construction creates exactly one swapchain");

    // `select::surface_format`'s choice, asserted against a *real* surface's real list rather
    // than only against the synthetic ones in `select.rs`. An `_SRGB` swapchain here would make
    // the driver encode guest pixels that are already encoded.
    let format = renderer.swapchain_format().expect("there is a swapchain");
    assert!(
        matches!(
            format,
            vk::Format::B8G8R8A8_UNORM
                | vk::Format::R8G8B8A8_UNORM
                | vk::Format::A8B8G8R8_UNORM_PACK32
        ),
        "expected an unencoded 8-bit format, got {format:?}"
    );
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn cleared_frames_are_presented_and_the_counters_agree_with_the_outcomes() {
    require_gate();
    let (mut window, mut renderer) = harness("omnidroid: clear", 640, 480);
    window.show();

    let target = 60;
    let outcomes = drive(&mut window, &mut renderer, "60 frames", |r| {
        r.frames_presented() >= target
    });

    // The counters are asserted as a **relation** to what the calls returned, not against a fixed
    // number of frames. That matters here specifically: the spike measured this host's surface
    // extent drifting across 41 spontaneous swapchain recreations in five seconds with the window
    // untouched (§4, a Parsec virtual display renegotiating), so any assertion of the form "no
    // recreations happened" would be a flake waiting for a slow run. What cannot drift is that
    // every recreation the renderer performed was one it told the caller about.
    assert_eq!(
        renderer.frames_presented(),
        outcomes.presented,
        "every Presented outcome is one frame and no frame is counted without one"
    );
    assert_eq!(
        renderer.swapchain_generations(),
        1 + outcomes.recreated,
        "the renderer must not rebuild a swapchain without returning SwapchainRecreated"
    );
    assert_eq!(outcomes.skipped, 0, "a visible window is never skipped");
    assert!(outcomes.total() >= target, "{outcomes:?}");
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn six_consecutive_resizes_are_survived_and_the_swapchain_follows_each_one() {
    require_gate();
    // **This is the regression test for the one measured bug in this stack.** The graphics spike's
    // first `recreate_swapchain` destroyed the outgoing `VkSwapchainKHR` before passing it as
    // `oldSwapchain`, and on this host — with no validation layers — that crashed the NVIDIA
    // driver on the very first live resize, every time (`nvoglv64.dll`, `0xc0000409`,
    // `docs/research/graphics-spike.md` §1). The sizes below are the spike's own: it verified the
    // fix with six consecutive live resizes from 500x400 up to 1250x900.
    let (mut window, mut renderer) = harness("omnidroid: resize", 500, 400);
    window.show();
    drive(&mut window, &mut renderer, "the first frame", |r| r.frames_presented() >= 1);

    let sizes = [(700, 500), (900, 700), (1100, 800), (1250, 900), (640, 480), (500, 400)];
    let mut generations = renderer.swapchain_generations();
    for (width, height) in sizes {
        window.set_client_size(width, height).unwrap();
        let before = renderer.frames_presented();
        let outcomes = drive(&mut window, &mut renderer, "the resized swapchain", |r| {
            r.swapchain_extent() == Some((width, height)) && r.frames_presented() > before
        });

        assert_eq!(
            renderer.swapchain_extent(),
            Some((width, height)),
            "the swapchain must follow the window to {width}x{height}"
        );
        assert_eq!(
            window.client_size().unwrap(),
            (width, height),
            "and the window must actually be that size — the swapchain agreeing with a size the \
             window does not have would be two wrongs agreeing"
        );
        assert!(
            outcomes.recreated >= 1,
            "a resize to {width}x{height} must have rebuilt the swapchain: {outcomes:?}"
        );
        assert_eq!(
            renderer.swapchain_generations(),
            generations + outcomes.recreated,
            "every rebuild must be one the renderer reported"
        );
        generations = renderer.swapchain_generations();
        assert!(renderer.frames_presented() > before, "a frame must reach the new swapchain");
    }
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_minimised_window_is_skipped_and_recovers_when_it_is_restored() {
    require_gate();
    // Vulkan refuses a zero-extent swapchain, so a minimised window must leave the renderer
    // holding none at all. This is a real production path — the user clicks minimise — and
    // `Window::set_minimized` exists so that it is not a path only a person can enter.
    let (mut window, mut renderer) = harness("omnidroid: minimise", 640, 480);
    window.show();
    drive(&mut window, &mut renderer, "the first frame", |r| r.frames_presented() >= 1);

    window.set_minimized(true).unwrap();
    let outcomes = drive(&mut window, &mut renderer, "the minimised state", |r| {
        r.swapchain_extent().is_none()
    });
    assert!(outcomes.skipped >= 1 || outcomes.recreated >= 1, "{outcomes:?}");
    assert_eq!(
        renderer.swapchain_extent(),
        None,
        "a minimised window has no pixels, so there must be no swapchain"
    );
    // And it must be a *skip*, not an error: presenting to a minimised window is something a
    // frame loop does dozens of times a second and must not tear the renderer down.
    assert_eq!(renderer.present_clear([0.0, 0.0, 0.0, 1.0]).unwrap(), FrameOutcome::Skipped);

    window.set_minimized(false).unwrap();
    let before = renderer.frames_presented();
    drive(&mut window, &mut renderer, "the restored window", |r| r.frames_presented() > before);
    assert_eq!(
        renderer.swapchain_extent(),
        Some(window.client_size().unwrap()),
        "restoring must rebuild the swapchain at the window's size"
    );
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn an_rgba8_frame_is_presented_at_every_size_relative_to_the_window() {
    require_gate();
    let (mut window, mut renderer) = harness("omnidroid: rgba8", 640, 480);
    window.show();
    drive(&mut window, &mut renderer, "the first frame", |r| r.frames_presented() >= 1);

    // Smaller than the window, exactly its size, and larger: the blit has to scale up, copy
    // one-to-one, and scale down. The staging pair is reallocated each time the dimensions
    // change, which is the other thing being exercised — and `vkDeviceWaitIdle` before that
    // reallocation is what keeps a frame in flight from reading a freed image.
    for (width, height) in [(64u32, 64u32), (640, 480), (1280, 960), (1, 1)] {
        let pixels = checkerboard(width, height);
        let image = Rgba8Image::new(width, height, &pixels).unwrap();
        let before = renderer.frames_presented();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut window, &mut renderer);
            let outcome = renderer
                .present_rgba8(&image)
                .unwrap_or_else(|err| panic!("presenting a {width}x{height} frame failed: {err}"));
            if outcome == FrameOutcome::Presented && renderer.frames_presented() > before {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a {width}x{height} frame never reached the screen; last outcome {outcome:?}"
            );
        }
    }

    // Alternating back to a cleared frame must work too: the staging pair stays allocated and
    // unused, and the two present paths share every object below the command buffer.
    assert_eq!(renderer.present_clear([1.0, 0.0, 1.0, 1.0]).unwrap(), FrameOutcome::Presented);
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_renderer_can_be_torn_down_and_rebuilt_on_the_same_window() {
    require_gate();
    // Teardown order is the thing with no validation layer watching it, and the symptom of
    // getting it wrong is not a failure here but a crash *later*, in an unrelated test, when the
    // driver trips over a leaked surface. Building a second renderer on the same `HWND` is the
    // closest a test can get: `vkCreateWin32SurfaceKHR` on a window that still has a live surface
    // is what `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR` is for, so a leaked surface fails loudly here.
    let mut window = Window::new(&WindowDesc::new("omnidroid: rebuild", 480, 360)).unwrap();
    let _ = window.poll_events().count();
    let size = window.client_size().unwrap();

    for round in 0..3 {
        let mut renderer = Renderer::new(window.raw(), size, RendererConfig::default())
            .unwrap_or_else(|err| panic!("round {round}: {err}"));
        drive(&mut window, &mut renderer, "a frame", |r| r.frames_presented() >= 1);
        assert_eq!(renderer.swapchain_generations(), 1, "round {round} built one swapchain");
        drop(renderer);
    }
}

#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn an_unavailable_present_mode_does_not_prevent_the_renderer_from_starting() {
    require_gate();
    // All three of this seam's modes were granted on this host (spike §4), so the *fallback* is
    // not reachable here — `select.rs` tests that arm synthetically. What this asserts is the
    // half that needs a real surface: whichever mode comes back is one the surface actually
    // offered, and asking for a non-default one does not break construction.
    let mut window = Window::new(&WindowDesc::new("omnidroid: present mode", 320, 240)).unwrap();
    let _ = window.poll_events().count();
    let size = window.client_size().unwrap();

    for mode in PresentMode::ALL {
        let renderer =
            Renderer::new(window.raw(), size, RendererConfig { present_mode: mode })
                .unwrap_or_else(|err| panic!("{mode:?}: {err}"));
        let got = renderer.present_mode();
        assert!(
            got == mode.to_vk() || got == vk::PresentModeKHR::FIFO,
            "asking for {mode:?} produced {got:?}, which is neither it nor the FIFO fallback"
        );
        assert!(renderer.swapchain_extent().is_some(), "{mode:?} produced no swapchain");
    }
}

/// A checkerboard, so that a presented frame is visibly *something* rather than a flat colour.
///
/// Not read back — see this file's header for why that is impossible on this host — but a human
/// running these with the window visible can see whether the blit scaled, and a flat fill could
/// not tell them that.
fn checkerboard(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        for x in 0..width {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let value = if on { 220 } else { 40 };
            pixels.extend_from_slice(&[value, u8::try_from(x % 256).unwrap_or(0), value, 255]);
        }
    }
    pixels
}
