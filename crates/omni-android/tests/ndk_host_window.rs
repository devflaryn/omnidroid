//! **`ANativeWindow` over a real host window**, with real translated ARM64 code asking for the
//! size and real Vulkan frames going into the thing it is the size of.
//!
//! ```text
//! cargo test -p omni-android --release --test ndk_host_window
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test ndk_host_window -- --ignored --test-threads=1
//! ```
//!
//! # What this file is for
//!
//! `tests/ndk.rs` proves the `ANativeWindow` contract against a geometry the host *asserted* —
//! 1440x3120, a constant with nothing behind it — and `tests/gameactivity.rs` drives the real
//! engine against 1280x720, another one. Both are [`WindowBacking::Fixed`]. This file is the
//! other backing: [`WindowBacking::Live`], where the number the guest reads came out of
//! `GetClientRect` on a window that is on the screen.
//!
//! # A test and an ignored test, and why it is both rather than an example
//!
//! Two of these run in the ordinary suite and two do not, and the split is the same one
//! `omni-gfx`'s `renderer_live.rs` makes for the same reason.
//!
//! The **precedence and refusal** tests need no window, no display and no GPU: they feed a
//! [`HostWindowSource`] by hand through [`HostWindowSource::publish`] and assert what the guest
//! reads back through a real thunk. Those are the regression detectors — they are what fails if
//! the live backing stops outranking the constant, or starts answering a size for a window that
//! has none — so they must run every time, on every machine, including the four targets with no
//! window backend at all.
//!
//! The **end-to-end** tests open a real resizable window, build a real Vulkan device and present
//! real frames into it. A build machine may have neither a desktop session nor a GPU, so
//! VERIFICATION entry 4 applies exactly as it does in `renderer_live.rs`: they are `#[ignore]`d,
//! so an ordinary run prints a line saying they did not run, and under `--ignored` they
//! **panic** naming `OMNI_GFX_WINDOW_TESTS` if nobody opted in. Nothing here is allowed to
//! quietly pass on a machine that cannot run it.
//!
//! An **example** was the other option and is worse for one reason: an example is not run by
//! anything. `cargo test` does not execute it, so an example that stopped working would keep
//! compiling and keep being cited, and the claim this file makes — the guest is told the real
//! window's real size — would have no way to fail. `cargo build --workspace` checking that it
//! still compiles is not a check that it still works.
//!
//! # What these can and cannot see
//!
//! They see that `ANativeWindow_getWidth` answers what `GetClientRect` says, that it keeps
//! answering through the **same** `ANativeWindow *` after the window is resized with nobody
//! calling `Ndk::set_window_geometry`, and that the swapchain frames are going into agrees with
//! that number. They cannot see what is on the screen: the graphics spike measured
//! `PrintWindow(PW_RENDERFULLCONTENT)` returning solid black for the client area of a flip-model
//! swapchain on this host (`docs/research/graphics-spike.md` §1), so there is no read-back of a
//! presented pixel anywhere in this workspace and this file does not pretend otherwise.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::jni::Jni;
use omni_android::ndk::{
    HostWindowSource, Ndk, WindowBacking, WindowGeometry, WindowSource, SURFACE_CLASS,
};
use omni_android::{AbiError, Boundary};
use omni_cpu::ExitReason;
use omni_gfx::vulkan::{FrameOutcome, Renderer, RendererConfig};
use omni_platform::window::{Window, WindowDesc, WindowEvent};

/// The opt-in, shared with `omni-gfx`'s `renderer_live.rs` and `omni-platform`'s `window_live.rs`:
/// one decision by one person covers the whole graphics bring-up rather than three.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// Fail, naming the variable, if an end-to-end test was run without the opt-in.
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

// =================================================================== the guest side

/// A guest, the three instances, and the boundary with everything bound.
///
/// The same arrangement `tests/ndk.rs` uses, and for its reason: every assertion here is about
/// the value **the guest got back** from a real thunk reached by real translated ARM64 code. A
/// test that called the handler directly would assert facts about a function call.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    ndk: Arc<Ndk>,
    boundary: Arc<Boundary>,
    _root: Scratch,
}

/// A host directory that removes itself, so the instance has a filesystem root.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-hostwin-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(tag: &str) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let ndk = Ndk::new(Arc::clone(&guest.space)).expect("an NDK instance");
    let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
    let builder = guest.boundary(640);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    ndk.bind_into(&builder).expect("bind every NDK handler");
    jni.install_into(&builder).expect("install the JNI tables");
    bionic.set_log_to_stderr(false);
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&guest.backend) as _;
    bionic.set_thread_host(ThreadHost::new(host)).expect("a thread host");
    let boundary = builder.finish();
    Fixture { guest, bionic, jni, ndk, boundary, _root: root }
}

impl Fixture {
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    fn run(&self, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
        let _ndk = self.ndk.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    fn program_calling(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
        let thunk = self.thunk(symbol);
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        setup(&mut asm);
        asm.bl(thunk);
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        entry
    }

    /// Assemble a program that ends by storing `X0` at `data`, run it, and return the value.
    fn value_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> u64 {
        let entry = self.program_calling(symbol, setup);
        let exit = self.run(entry).expect("the run must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        self.guest.read_u64(self.guest.data)
    }

    /// The refusal a one-call program produced.
    fn refusal_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> AbiError {
        let entry = self.program_calling(symbol, setup);
        match self.run(entry) {
            Err(error) => error,
            Ok(exit) => panic!("`{symbol}` completed with {exit:?} where a refusal was required"),
        }
    }

    /// `ANativeWindow_fromSurface(NULL, surface)` through the guest, on a fresh `Surface`.
    fn a_window(&self) -> u64 {
        let surface = self.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
        let window = self.value_of("ANativeWindow_fromSurface", |asm| {
            asm.mov(0, 0);
            asm.mov(1, surface);
        });
        assert_ne!(window, 0, "fromSurface needs no geometry and must produce a window");
        window
    }

    /// What the **guest** reads from `ANativeWindow_getWidth` and `_getHeight`, as the `int32_t`
    /// pair they are.
    ///
    /// Both, always, and never one: a handler that returned the width for both would pass every
    /// assertion made with a square window (`tools/mutate.py` row `window-A2`), so nothing here
    /// ever uses one without the other and no window in this file is square.
    fn guest_size(&self, window: u64) -> (i32, i32) {
        let width = self.value_of("ANativeWindow_getWidth", |asm| {
            asm.mov(0, window);
        }) as i32;
        let height = self.value_of("ANativeWindow_getHeight", |asm| {
            asm.mov(0, window);
        }) as i32;
        (width, height)
    }
}

/// Whether the instance's backing is the live one. The **detector**, not the number: a guest
/// reading 1280x720 proves nothing on its own, because a constant can say 1280x720 too.
fn backing_is_live(ndk: &Ndk) -> bool {
    matches!(ndk.window_backing(), Some(WindowBacking::Live(_)))
}

// =================================================================== no window needed

/// **A live source outranks a constant, in either order, and `window_backing` says which
/// answered.**
///
/// This is the rule `ndk::window` states and the one nothing else could catch: both backings can
/// hold the same number, so a test that only read the width back would be green against an
/// instance that had quietly gone on answering the constant. The numbers here are therefore
/// deliberately far apart, and the backing is asserted as well as the value.
///
/// It runs in the ordinary suite because it needs no window: [`HostWindowSource::publish`] is fed
/// by hand, which is the same path [`HostWindowSource::sample`] ends in after it has asked the
/// OS.
#[test]
fn a_live_source_outranks_a_constant_and_the_backing_says_which_answered() {
    let _guard = serialized();
    let f = fixture("precedence");
    let window = f.a_window();

    // Constant first, source second.
    f.ndk.set_window_geometry(WindowGeometry::new(1440, 3120).expect("a positive geometry"));
    assert!(!backing_is_live(&f.ndk), "a constant alone is not a live backing");
    assert_eq!(f.guest_size(window), (1440, 3120), "the constant answers while it is alone");

    let source = HostWindowSource::unpublished();
    source.publish(800, 600).expect("a publishable size");
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);
    assert!(backing_is_live(&f.ndk), "the live source is what answers now");
    assert_eq!(
        f.guest_size(window),
        (800, 600),
        "the live source outranks the constant the host set before it"
    );
    assert_eq!(
        f.ndk.window_geometry(),
        Some(WindowGeometry::new(1440, 3120).expect("a positive geometry")),
        "the constant is still there and still readable; it is outranked, not erased"
    );

    // The other order, on a second instance: source first, constant afterwards. A host that
    // contradicts itself does not get to change the answer by being late.
    let g = fixture("precedence-reversed");
    let other = g.a_window();
    let live = HostWindowSource::unpublished();
    live.publish(800, 600).expect("a publishable size");
    g.ndk.set_window_source(Arc::clone(&live) as Arc<dyn WindowSource>);
    g.ndk.set_window_geometry(WindowGeometry::new(1440, 3120).expect("a positive geometry"));
    assert!(backing_is_live(&g.ndk));
    assert_eq!(g.guest_size(other), (800, 600), "precedence is by kind, not by which call is last");

    // And a resize through the source reaches the window the guest is already holding -- the
    // property `tests/ndk.rs` asserts for the constant, asserted for the live backing too.
    live.publish(1024, 480).expect("a publishable size");
    assert_eq!(g.guest_size(other), (1024, 480), "no new fromSurface, no new handle, new size");
}

/// **A live source with no client area to report refuses by name**, and the refusal tells an
/// unfed source from a window that really has no pixels.
///
/// A minimised window is the production case: its client area is 0x0, `WindowGeometry` refuses a
/// non-positive axis because a surface has a positive extent in both, and there is no device
/// analogue to borrow — Android destroys the surface instead of shrinking it. So the honest
/// answer is a refusal naming the source, and the two host states behind it have different fixes,
/// which is why `HostWindowSource`'s `Debug` distinguishes them and the refusal carries it.
#[test]
fn a_source_with_no_client_area_refuses_and_names_which_host_state_it_is() {
    let _guard = serialized();
    let f = fixture("no-pixels");
    let window = f.a_window();
    let source = HostWindowSource::unpublished();
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    for symbol in ["ANativeWindow_getWidth", "ANativeWindow_getHeight"] {
        let error = f.refusal_of(symbol, |asm| {
            asm.mov(0, window);
        });
        assert_eq!(error.symbol(), Some(symbol));
        let text = error.to_string();
        assert!(text.contains("HostWindowSource"), "the refusal names the source: {error}");
        assert!(text.contains("nothing published yet"), "{error}");
    }

    // Now the source has been fed a minimised window's 0x0. Still a refusal, and a different
    // one: this host has a window, it just has no pixels right now.
    source.publish(0, 0).expect("a minimised window publishes");
    let error = f.refusal_of("ANativeWindow_getWidth", |asm| {
        asm.mov(0, window);
    });
    let text = error.to_string();
    assert!(text.contains("no pixels"), "{error}");
    assert!(!text.contains("nothing published yet"), "a fed source is not an unfed one: {error}");

    // Restored. The same handle answers again, with nobody having called fromSurface.
    source.publish(1024, 480).expect("a publishable size");
    assert_eq!(f.guest_size(window), (1024, 480));
}

// =================================================================== a real window, real frames

/// A window, a renderer for it, and a source watching it.
///
/// Deliberately **not square** and deliberately not 1920x1080: the first would hide a handler
/// that answered the width for the height, and the second is the value `ndk::window` names as the
/// believable wrong answer, which a test must never use as its expected value.
fn live_harness(title: &str) -> (Window, Renderer, Arc<HostWindowSource>) {
    let mut window = Window::new(&WindowDesc::new(title, 1024, 576))
        .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    // Drain creation's own resize burst so the loop below starts from a known queue.
    let _ = window.poll_events().count();
    let size = window.client_size().expect("the window's client size");
    let renderer = Renderer::new(window.raw(), size, RendererConfig::default())
        .unwrap_or_else(|err| panic!("could not create the renderer: {err}"));
    let source = HostWindowSource::watching(&window).expect("a source watching the window");
    (window, renderer, source)
}

/// One turn of a host frame loop: drain the window, tell the renderer, re-sample the source,
/// present.
///
/// The order is the order a real host must use and is the whole of what an embedding has to do.
/// `sample` is called **every** turn rather than only on a `Resized` event, which is the point of
/// a pull source: `Window::client_size` asks the OS because the graphics spike measured this
/// host's extent drifting 41 times in 5 seconds with no event to carry it
/// (`docs/research/graphics-spike.md` §4).
fn frame(window: &mut Window, renderer: &mut Renderer, source: &HostWindowSource) -> FrameOutcome {
    for event in window.poll_events() {
        if let WindowEvent::Resized { width, height } = event {
            renderer.notify_resized(width, height);
        }
    }
    source.sample(window).expect("the window can be asked for its client size");
    renderer.present_clear([0.02, 0.02, 0.08, 1.0]).expect("presenting a cleared frame")
}

/// Run the frame loop until `done`, or fail naming what was being waited for.
///
/// A bounded poll on the thing under test rather than a sleep (VERIFICATION entry 6).
fn drive(
    window: &mut Window,
    renderer: &mut Renderer,
    source: &HostWindowSource,
    what: &str,
    mut done: impl FnMut(&Renderer, &HostWindowSource) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        frame(window, renderer, source);
        if done(renderer, source) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited 10s for {what}; the renderer is {renderer:?} and the source is {source:?}"
        );
    }
}

/// **The guest reads the real window's real size, and follows a real resize, while real frames
/// are being presented into that window.**
///
/// This is the claim the whole seam exists for, and every part of it is asserted against
/// something that can disagree:
///
/// * the size the guest read equals what `GetClientRect` reports **now**, asked again after the
///   guest was asked;
/// * it equals the extent of the swapchain the frames are actually going into, so "the guest's
///   surface" and "the surface" are the same surface and not two numbers that started equal;
/// * after the window is resized it equals the **new** size, through the **same**
///   `ANativeWindow *`, with `Ndk::set_window_geometry` never called — `window_geometry()` is
///   asserted `None` at the end, so the constant path cannot be what answered;
/// * frames were presented on both sides of the resize, and the swapchain was recreated, so the
///   window was live rather than merely open.
#[test]
#[ignore = "opens a real window and presents real Vulkan frames; see this file's header"]
fn the_guest_reads_the_real_windows_size_and_follows_a_resize() {
    require_gate();
    let _guard = serialized();
    let (mut window, mut renderer, source) = live_harness("Omnidroid - ANativeWindow_getWidth");

    let f = fixture("live-window");
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);
    assert!(backing_is_live(&f.ndk), "the instance must be on the live backing, not a constant");
    let handle = f.a_window();

    // Get a real frame on the screen before believing anything about the window.
    drive(&mut window, &mut renderer, &source, "the first presented frames", |r, _| {
        r.frames_presented() >= 3
    });
    let first_frames = renderer.frames_presented();
    let first_generations = renderer.swapchain_generations();
    assert!(first_generations >= 1, "a presented frame implies a swapchain");

    let guest_before = f.guest_size(handle);
    let host_before = window.client_size().expect("the window's client size");
    assert_eq!(
        guest_before,
        (host_before.0 as i32, host_before.1 as i32),
        "the guest read {guest_before:?} and GetClientRect says {host_before:?}"
    );
    assert_eq!(
        renderer.swapchain_extent(),
        Some(host_before),
        "the frames are going into a swapchain of the size the guest was told"
    );
    assert_ne!(guest_before.0, guest_before.1, "a square window cannot tell the axes apart");

    // The resize. `set_client_size` is a *request*: Windows clamps and enforces a minimum
    // tracking size, so what is asserted is that the window changed and that the guest followed
    // it -- not that it became exactly these numbers.
    window.set_client_size(736, 414).expect("the window accepts a resize request");
    drive(&mut window, &mut renderer, &source, "the resize to reach the swapchain", |r, s| {
        r.swapchain_generations() > first_generations
            && s.geometry().is_some()
            && r.swapchain_extent().is_some()
    });
    drive(&mut window, &mut renderer, &source, "frames after the resize", |r, _| {
        r.frames_presented() > first_frames + 2
    });

    let host_after = window.client_size().expect("the window's client size");
    assert_ne!(host_after, host_before, "the window did not actually resize; nothing was tested");
    let guest_after = f.guest_size(handle);
    assert_eq!(
        guest_after,
        (host_after.0 as i32, host_after.1 as i32),
        "the guest read {guest_after:?} after a resize to {host_after:?}, through the same \
         ANativeWindow * and with nobody calling Ndk::set_window_geometry"
    );
    assert_eq!(
        renderer.swapchain_extent(),
        Some(host_after),
        "after the resize the guest's size and the presented surface's size are still one number"
    );

    assert_eq!(
        f.ndk.window_geometry(),
        None,
        "no constant was ever set, so the live source is provably what answered"
    );
    assert!(backing_is_live(&f.ndk));
    assert!(
        source.samples() > 4,
        "the source is sampled every frame; {} samples is not a frame loop",
        source.samples()
    );
    assert_eq!(
        f.ndk.census().get("ANativeWindow_getWidth").copied(),
        Some(2),
        "two getWidth calls were made through the guest and both must be counted"
    );

    // Printed rather than only asserted, under `--nocapture`, because this is the one run in the
    // workspace that can say what hardware answered: a green line says the assertions held, and
    // these numbers say what they held against.
    println!(
        "live window: device {:?}, validation {}, guest {guest_before:?} -> {guest_after:?}, \
         swapchain {:?} over {} generations, {} frames presented, {} samples",
        renderer.device().name,
        renderer.validation_enabled(),
        renderer.swapchain_extent(),
        renderer.swapchain_generations(),
        renderer.frames_presented(),
        source.samples()
    );

    // The renderer holds a surface on this window, and a surface outliving its window is
    // undefined behaviour in Vulkan. Tear down in that order explicitly rather than relying on
    // the order of the locals.
    drop(renderer);
    drop(window);
}

/// **A minimised real window refuses by name, and the same handle answers again when it comes
/// back.**
///
/// The zero-extent path with a real window rather than a hand-published 0x0: `ShowWindow`
/// produces a genuine 0x0 client area, the renderer answers [`FrameOutcome::Skipped`] because
/// Vulkan cannot build a swapchain for it, and `ANativeWindow_getWidth` refuses rather than
/// inventing a size for a surface that has none. VERIFICATION entry 12 is why this is worth a
/// test at all: without `Window::set_minimized` it is a branch only a person clicking a button
/// can reach.
#[test]
#[ignore = "opens a real window and presents real Vulkan frames; see this file's header"]
fn a_minimised_real_window_refuses_and_recovers_on_the_same_handle() {
    require_gate();
    let _guard = serialized();
    let (mut window, mut renderer, source) = live_harness("Omnidroid - minimised ANativeWindow");

    let f = fixture("live-minimise");
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);
    let handle = f.a_window();
    drive(&mut window, &mut renderer, &source, "the first presented frames", |r, _| {
        r.frames_presented() >= 2
    });
    let restored = f.guest_size(handle);
    assert!(restored.0 > 0 && restored.1 > 0, "the window has pixels before it is minimised");

    window.set_minimized(true).expect("the window can be minimised");
    drive(&mut window, &mut renderer, &source, "the window to report no pixels", |_, s| {
        s.geometry().is_none()
    });
    assert_eq!(
        renderer.swapchain_extent(),
        None,
        "a minimised window has no swapchain: Vulkan rejects a zero extent"
    );
    let error = f.refusal_of("ANativeWindow_getWidth", |asm| {
        asm.mov(0, handle);
    });
    assert_eq!(error.symbol(), Some("ANativeWindow_getWidth"));
    let text = error.to_string();
    assert!(text.contains("no pixels"), "{error}");
    assert!(text.contains("HostWindowSource"), "the refusal names the source: {error}");

    window.set_minimized(false).expect("the window can be restored");
    drive(&mut window, &mut renderer, &source, "the window to report pixels again", |r, s| {
        s.geometry().is_some() && r.swapchain_extent().is_some()
    });
    let after = f.guest_size(handle);
    assert!(after.0 > 0 && after.1 > 0, "the same handle answers again: {after:?}");
    assert_eq!(
        after,
        {
            let (w, h) = window.client_size().expect("the window's client size");
            (w as i32, h as i32)
        },
        "and it answers the restored window's real size"
    );

    drop(renderer);
    drop(window);
}
