//! The display's composer, served from the host: `android.hardware.graphics.composer3.IComposer`
//! (AIDL V3), what SurfaceFlinger composes through (D3b design,
//! `docs/superpowers/specs/2026-09-27-d3b-composer-design.md`).
//!
//! One display. When every layer of a frame is one it can compose itself (`super::compose`: RGBA
//! buffers at 1:1 with no transform, solid colours), the composer keeps them `DEVICE` and blends
//! them from their gralloc regions (D2) into the host [`Framebuffer`] at present: SurfaceFlinger's
//! RenderEngine draws nothing. Otherwise every layer is changed to `CLIENT`, RenderEngine composes
//! them (on the host GPU, D3a) into the client target, and presenting copies that target.
//! `OMNI_COMPOSER_DEVICE=0` makes every frame `CLIENT`. Composition is synchronous, so there are no
//! fences to report.
//!
//! **The display can be resized** ([`Composer::set_display_size`]), as an external display whose
//! mode changes is: the composer offers one configuration of the new size under a new id and
//! reports the display connected again (`onHotplug`). SurfaceFlinger takes that as a reconnect
//! ("Reconnecting ..."): it reloads the display's modes, recreates the display at the new size and
//! tells DisplayManager, whose LocalDisplayAdapter updates the display device -- and from there
//! WindowManager and every app get the configuration change a resized display causes on a device.
//!
//! **Only the app is presented** (the default; `OMNI_APP_ONLY=0` or
//! [`Composer::set_show_chrome`] shows the whole display): the system's chrome -- SystemUI's status
//! and navigation bars and screen decorations, the launcher's taskbar -- are layers like any other
//! here, and the composer leaves them out of what it presents. It knows them by the window their
//! buffers are for: a window's BufferQueue names its buffers after it when it asks the allocator
//! for them (`VRI[StatusBar]#0(BLAST Consumer)0`; `hal::gralloc` keeps the name, [`is_chrome`]).
//! A chrome layer stays `DEVICE`, which is the composer's to draw -- and it draws nothing. When the
//! frame's other layers are the composer's too, they are composed as they are; when one is not,
//! they go to SurfaceFlinger (`CLIENT`), whose client target then holds everything but the chrome,
//! with the app's own pixels where the chrome would have been on top of them. The app's layer is
//! not moved or scaled: an app that draws edge to edge (a game; any app on a device without
//! SystemUI) fills the display, and one that keeps clear of the bars shows its own background there.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use super::aidl::android_hardware_common::NativeHandle;
use super::aidl::android_hardware_graphics_common as common;
use super::aidl::android_hardware_graphics_composer3::{
    i_composer, i_composer_client, Capability, ChangedCompositionLayer, ChangedCompositionTypes, ColorMode, CommandError, CommandResultPayload, Composition,
    DisplayAttribute, DisplayCapability, DisplayCommand, DisplayConfiguration, DisplayConfiguration_Dpi, DisplayConnectionType, HdrCapabilities,
    IComposerCallbackProxy, IComposerClientServer, IComposerServer, PerFrameMetadataKey, PowerMode, PresentOrValidate, PresentOrValidate_Result,
    RenderIntent, ContentType, ClockMonotonicTimestamp, VsyncPeriodChangeConstraints, VsyncPeriodChangeTimeline,
};
use super::aidl::{Binder, Ctx, Fd, Status};
use super::framebuffer::Framebuffer;
use super::gralloc::{NAME_AT, PIXELS_AT};
use crate::binder::{Broker, STABILITY_VINTF};
use crate::fd::FileKind;
use crate::shm::Shm;

/// The instance SurfaceFlinger waits for (declared by the image's `hwc3.xml`).
pub const INSTANCE: &str = "android.hardware.graphics.composer3.IComposer/default";

/// The one display's id, initial size, refresh and density.
const DISPLAY: i64 = 0;
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// The smallest display [`Composer::set_display_size`] makes, in either axis: Android's smallest
/// screen width is 320 dp (the CDD's minimum), 320 pixels at this display's 160 dpi.
pub const MIN_SIDE: u32 = 320;
const VSYNC_PERIOD_NS: i32 = 16_666_666;
const DPI: f32 = 160.0;

/// `IComposerClient`'s service-specific errors.
const EX_BAD_DISPLAY: i32 = 2;
const EX_BAD_LAYER: i32 = 3;
const EX_NO_RESOURCES: i32 = 6;
const EX_UNSUPPORTED: i32 = 8;

/// The gralloc handle's ints (`hal::gralloc`): the format at 5, the stride at 8, the pixels at 13.
const HANDLE_INTS: usize = 14;
const RGBA_8888: i32 = 1;
const RGBX_8888: i32 = 2;
const BGRA_8888: i32 = 5;
const IMPLEMENTATION_DEFINED: i32 = 0x22;

/// The display's one configuration: its id and size. A resize is a new id, so that nothing
/// SurfaceFlinger or DisplayManager kept of the old mode can be mistaken for the new one.
#[derive(Debug, Clone, Copy)]
struct Mode {
    config: i32,
    width: u32,
    height: u32,
}

/// The windows that are the system's chrome, by the start of their title: SystemUI's bars and
/// screen decorations (rounded corners, cutout), and the launcher's taskbar.
const CHROME: &[&str] = &["StatusBar", "NavigationBar", "Taskbar", "ScreenDecorOverlay", "ScreenDecorHwcLayer"];

/// Whether a buffer of this name (`hal::gralloc`'s metadata page) is one of the system's chrome:
/// a view root's BufferQueue is named `VRI[<window title>]#<n>(BLAST Consumer)<n>`, and the title
/// is one of [`CHROME`]'s.
#[must_use]
pub fn is_chrome(buffer_name: &str) -> bool {
    let Some(title) = buffer_name.strip_prefix("VRI[").and_then(|rest| rest.split(']').next()) else { return false };
    CHROME.iter().any(|c| title.starts_with(c))
}

pub struct Composer {
    broker: Arc<Broker>,
    framebuffer: Arc<Framebuffer>,
    client: Mutex<Option<Arc<Client>>>,
    mode: Arc<Mutex<Mode>>,
    /// Whether the system's chrome is presented (see this module's "Only the app").
    show_chrome: Arc<AtomicBool>,
}

impl Composer {
    #[must_use]
    pub fn new(broker: Arc<Broker>, framebuffer: Arc<Framebuffer>) -> Arc<Self> {
        let (width, height) = framebuffer.size();
        let mode = Arc::new(Mutex::new(Mode { config: 0, width, height }));
        let show_chrome = std::env::var("OMNI_APP_ONLY").as_deref() == Ok("0");
        eprintln!("[composer] {}", if show_chrome { "the whole display is presented (OMNI_APP_ONLY=0)" } else { "only the app is presented: the system's bars and taskbar are left out" });
        Arc::new(Self { broker, framebuffer, client: Mutex::new(None), mode, show_chrome: Arc::new(AtomicBool::new(show_chrome)) })
    }

    /// Whether the system's chrome is presented.
    #[must_use]
    pub fn shows_chrome(&self) -> bool {
        self.show_chrome.load(Ordering::Relaxed)
    }

    /// Present the system's chrome with the app, or the app alone (see this module's "Only the
    /// app"), from the next frame on -- which is asked for at once (`onRefresh`), so that a still
    /// screen changes too.
    pub fn set_show_chrome(&self, show: bool) {
        if self.show_chrome.swap(show, Ordering::Relaxed) == show {
            return;
        }
        eprintln!("[composer] {}", if show { "the whole display is presented" } else { "only the app is presented" });
        let callback = self.client.lock().as_ref().and_then(|c| c.state.lock().callback);
        if let Some(callback) = callback {
            let _ = IComposerCallbackProxy::new(Arc::clone(&self.broker), callback).on_refresh(DISPLAY);
        }
    }

    /// The display's size now.
    #[must_use]
    pub fn display_size(&self) -> (u32, u32) {
        let m = *self.mode.lock();
        (m.width, m.height)
    }

    /// **Resize the display** to `width` x `height` (each at least [`MIN_SIDE`]): a configuration
    /// of that size under a new id, and the display reported connected again -- see this module's
    /// doc. Answers the size the display now has. Before SurfaceFlinger has registered its
    /// callback the size is only recorded, for its first look at the display.
    ///
    /// # Errors
    /// The hotplug's delivery failed (SurfaceFlinger gone).
    pub fn set_display_size(&self, width: u32, height: u32) -> Result<(u32, u32), String> {
        let (width, height) = (width.max(MIN_SIDE), height.max(MIN_SIDE));
        let config = {
            let mut m = self.mode.lock();
            if (m.width, m.height) == (width, height) {
                return Ok((width, height));
            }
            *m = Mode { config: m.config + 1, width, height };
            m.config
        };
        let callback = self.client.lock().as_ref().and_then(|c| c.state.lock().callback);
        eprintln!("[composer] display {width}x{height} (config {config}): {}", if callback.is_some() { "hotplug" } else { "before SurfaceFlinger" });
        if let Some(callback) = callback {
            IComposerCallbackProxy::new(Arc::clone(&self.broker), callback).on_hotplug(DISPLAY, true).map_err(|e| format!("onHotplug: {e:?}"))?;
        }
        Ok((width, height))
    }

    /// Serve the composer and publish it with `servicemanager` as [`INSTANCE`].
    ///
    /// # Errors
    /// `servicemanager`'s refusal.
    pub fn register(self: &Arc<Self>) -> Result<(), String> {
        let me = Arc::clone(self);
        let ptr = self.broker.create_host_service_objects(move |call| i_composer::dispatch(&*me, call));
        self.broker.add_service_with_stability(INSTANCE, ptr, STABILITY_VINTF)
    }
}

impl IComposerServer for Composer {
    fn create_client(&self, _ctx: &Ctx<'_>) -> Result<Binder, Status> {
        let mut client = self.client.lock();
        if client.is_some() {
            // One client at a time, as the interface says.
            return Err(Status::ServiceSpecific(EX_NO_RESOURCES));
        }
        let c = Client::new(Arc::clone(&self.broker), Arc::clone(&self.framebuffer), Arc::clone(&self.mode), Arc::clone(&self.show_chrome));
        let serve = Arc::clone(&c);
        let trace = std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("1");
        let ptr = self.broker.create_host_service_objects(move |call| {
            if trace {
                eprintln!("[composer] IComposerClient transaction {}", call.code);
            }
            i_composer_client::dispatch(&*serve, call)
        });
        *client = Some(c);
        Ok(Binder::Host(ptr))
    }

    fn get_capabilities(&self, _ctx: &Ctx<'_>) -> Result<Vec<Capability>, Status> {
        Ok(Vec::new())
    }
}

/// A client target the client has sent in some slot.
struct Target {
    shm: Arc<Shm>,
    format: i32,
    width: u32,
    height: u32,
    stride: u32,
    pixels_at: u64,
}

/// A buffer a layer has sent in some slot.
struct LayerBuffer {
    shm: Arc<Shm>,
    format: i32,
    stride: u32,
    width: u32,
    height: u32,
    pixels_at: u64,
    /// The name its requestor gave the allocator (`hal::gralloc`'s metadata page): a BufferQueue's
    /// consumer name, which names the window it is for.
    name: String,
}

/// What the client last set on a layer (a command carries only what changed).
#[derive(Default)]
struct LayerState {
    buffers: HashMap<i32, LayerBuffer>,
    slot: Option<i32>,
    frame: Option<common::Rect>,
    crop: Option<common::FRect>,
    blend: Option<common::BlendMode>,
    alpha: Option<f32>,
    z: i32,
    transform: Option<common::Transform>,
    color: Option<[f32; 4]>,
}

impl LayerState {
    /// Whether the composer can compose this layer itself as `composition`.
    fn composable(&self, composition: Composition) -> bool {
        let Some(frame) = &self.frame else { return false };
        if frame.right <= frame.left || frame.bottom <= frame.top {
            return true; // nothing to draw
        }
        if composition == Composition::SOLID_COLOR {
            return self.color.is_some();
        }
        if composition != Composition::DEVICE || self.transform.is_some_and(|t| !(0..=7).contains(&t.0)) {
            return false;
        }
        let (Some(buffer), Some(crop)) = (self.slot.and_then(|s| self.buffers.get(&s)), &self.crop) else { return false };
        matches!(buffer.format, RGBA_8888 | RGBX_8888 | BGRA_8888 | IMPLEMENTATION_DEFINED)
            && crop.left >= 0.0
            && crop.top >= 0.0
            && crop.right > crop.left
            && crop.bottom > crop.top
            && crop.right <= buffer.width as f32
            && crop.bottom <= buffer.height as f32
    }

    /// Whether its buffer is shown at 1:1 from a whole-pixel corner, untransformed, as stored: the
    /// fast path (rows copied or blended as they are).
    fn one_to_one(&self) -> bool {
        let (Some(f), Some(c), Some(b)) = (&self.frame, &self.crop, self.slot.and_then(|s| self.buffers.get(&s))) else { return false };
        self.transform.is_none_or(|t| t == common::Transform::NONE)
            && b.format != BGRA_8888
            && c.left.fract() == 0.0
            && c.top.fract() == 0.0
            && ((c.right - c.left) - (f.right - f.left) as f32).abs() < 0.01
            && ((c.bottom - c.top) - (f.bottom - f.top) as f32).abs() < 0.01
    }
}

#[derive(Default)]
struct State {
    callback: Option<u32>,
    next_layer: i64,
    /// Each layer and the composition its client last asked for.
    layers: HashMap<i64, Composition>,
    targets: HashMap<i32, Target>,
    current_target: Option<i32>,
    refused_format: Option<i32>,
    /// Each layer's properties, for the composer's own composition.
    device: HashMap<i64, LayerState>,
    /// Whether the frame being presented is the composer's own (`DEVICE`), decided at validation.
    device_frame: bool,
    /// The chrome layers left out of the frame being presented, decided at validation.
    hidden: std::collections::HashSet<i64>,
    /// Frames presented by each path, for the `[composer]` line.
    frames_device: u64,
    frames_client: u64,
    /// The layer list `OMNI_COMPOSER_TRACE=layers` last printed.
    traced: String,
}

pub struct Client {
    broker: Arc<Broker>,
    framebuffer: Arc<Framebuffer>,
    state: Mutex<State>,
    vsync: Arc<AtomicBool>,
    mode: Arc<Mutex<Mode>>,
    show_chrome: Arc<AtomicBool>,
}

impl Client {
    fn new(broker: Arc<Broker>, framebuffer: Arc<Framebuffer>, mode: Arc<Mutex<Mode>>, show_chrome: Arc<AtomicBool>) -> Arc<Self> {
        let c = Arc::new(Self { broker, framebuffer, state: Mutex::default(), vsync: Arc::default(), mode, show_chrome });
        // Vsync, every period while enabled, for as long as the client lives.
        let weak: Weak<Self> = Arc::downgrade(&c);
        let _ = std::thread::Builder::new().name("omni-composer-vsync".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_nanos(VSYNC_PERIOD_NS as u64));
            let Some(c) = weak.upgrade() else { return };
            if !c.vsync.load(Ordering::Relaxed) {
                continue;
            }
            let Some(callback) = c.state.lock().callback else { continue };
            let now = crate::sys::monotonic().as_nanos() as i64;
            let _ = IComposerCallbackProxy::new(Arc::clone(&c.broker), callback).on_vsync(DISPLAY, now, VSYNC_PERIOD_NS);
        });
        c
    }

    fn display(display: i64) -> Result<(), Status> {
        if display == DISPLAY { Ok(()) } else { Err(Status::ServiceSpecific(EX_BAD_DISPLAY)) }
    }

    fn mode(&self) -> Mode {
        *self.mode.lock()
    }

    /// Present the frame: the composer's own composition of its layers, or the client target.
    fn present(&self) {
        let mut st = self.state.lock();
        if st.device_frame {
            st.frames_device += 1;
            log_paths(&st);
            let mut order: Vec<(i32, i64)> = st.layers.keys().map(|l| (st.device.get(l).map_or(0, |d| d.z), *l)).collect();
            order.sort_unstable();
            // Each buffer layer's pixels, read whole from its region.
            let mut pixels: HashMap<i64, Vec<u8>> = HashMap::new();
            for &(_, l) in &order {
                let Some(d) = st.device.get(&l) else { continue };
                if st.layers.get(&l) != Some(&Composition::DEVICE) || st.hidden.contains(&l) {
                    continue;
                }
                if let Some(b) = d.slot.and_then(|s| d.buffers.get(&s)) {
                    let mut bytes = vec![0u8; b.stride as usize * b.height as usize * 4];
                    if b.shm.read_at(&mut bytes, b.pixels_at).is_ok() {
                        pixels.insert(l, bytes);
                    }
                }
            }
            let mut layers = Vec::new();
            for &(_, l) in &order {
                let Some(d) = st.device.get(&l) else { continue };
                let Some(f) = &d.frame else { continue };
                if st.hidden.contains(&l) {
                    continue;
                }
                let blend = match d.blend {
                    Some(common::BlendMode::NONE) => super::compose::Blend::None,
                    Some(common::BlendMode::COVERAGE) => super::compose::Blend::Coverage,
                    _ => super::compose::Blend::Premultiplied,
                };
                let source = if st.layers.get(&l) == Some(&Composition::SOLID_COLOR) {
                    super::compose::Source::Color(d.color.unwrap_or_default())
                } else {
                    let (Some(b), Some(data), Some(c)) = (d.slot.and_then(|s| d.buffers.get(&s)), pixels.get(&l), &d.crop) else { continue };
                    if d.one_to_one() {
                        super::compose::Source::Pixels { data, stride: b.stride as usize, opaque: b.format == RGBX_8888, crop_x: c.left as usize, crop_y: c.top as usize }
                    } else {
                        super::compose::Source::Mapped {
                            data,
                            stride: b.stride as usize,
                            rows: b.height as usize,
                            opaque: b.format == RGBX_8888,
                            bgra: b.format == BGRA_8888,
                            crop: (c.left, c.top, c.right, c.bottom),
                            transform: d.transform.map_or(0, |t| t.0 as u32),
                        }
                    }
                };
                layers.push(super::compose::Layer { source, frame: (f.left, f.top, f.right, f.bottom), blend, alpha: d.alpha.unwrap_or(1.0) });
            }
            let Mode { width, height, .. } = self.mode();
            let mut out = vec![0u8; width as usize * height as usize * 4];
            super::compose::compose(&mut out, width as usize, height as usize, &layers);
            drop(layers);
            drop(st);
            self.framebuffer.present_frame(&out, width, height, width);
            return;
        }
        st.frames_client += 1;
        log_paths(&st);
        let Some(target) = st.current_target.and_then(|slot| st.targets.get(&slot)) else { return };
        if !matches!(target.format, RGBA_8888 | RGBX_8888 | IMPLEMENTATION_DEFINED) {
            if st.refused_format != Some(target.format) {
                eprintln!("[composer] a client target of pixel format {:#x} is not presented (RGBA_8888 only)", target.format);
                st.refused_format = Some(target.format);
            }
            return;
        }
        // The target's own size: the display's, except for a frame SurfaceFlinger drew for the
        // size before a resize.
        let (width, height) = (target.width, target.height);
        let mut pixels = vec![0u8; target.stride as usize * height as usize * 4];
        if target.shm.read_at(&mut pixels, target.pixels_at).is_err() {
            return;
        }
        let stride = target.stride;
        // OMNI_COMPOSER_TRACE=2: each frame presented, its slot and two pixels (centre, corner).
        static FRAMES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FRAMES.get_or_init(|| std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("2")) {
            let px = |x: usize, y: usize| {
                let at = (y * stride as usize + x) * 4;
                u32::from_be_bytes(pixels[at..at + 4].try_into().expect("4"))
            };
            eprintln!("[composer] present slot {:?}: centre {:08x} corner {:08x}", st.current_target, px(width as usize / 2, height as usize / 2), px(8, 8));
        }
        drop(st);
        self.framebuffer.present_frame(&pixels, width, height, stride);
    }
}

/// `OMNI_COMPOSER_TRACE=layers`: the frame's layers, bottom first, each time their list or their
/// geometry changes -- what each is (its buffer's name), where, and whether the composer can take it.
fn trace_layers(st: &mut State) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("layers")) {
        return;
    }
    let mut order: Vec<(i32, i64)> = st.layers.keys().map(|l| (st.device.get(l).map_or(0, |d| d.z), *l)).collect();
    order.sort_unstable();
    let mut text = String::new();
    for (z, l) in order {
        let c = st.layers[&l];
        let Some(d) = st.device.get(&l) else {
            text.push_str(&format!("  layer {l} z {z} {c:?}: nothing set\n"));
            continue;
        };
        let buffer = d.slot.and_then(|s| d.buffers.get(&s)).map_or_else(
            || "no buffer".to_string(),
            |b| format!("{:?} {}x{} fmt {:#x} stride {}", b.name, b.width, b.height, b.format, b.stride),
        );
        let frame = d.frame.as_ref().map(|f| format!("[{},{} {},{}]", f.left, f.top, f.right, f.bottom));
        let crop = d.crop.as_ref().map(|c| format!("[{},{} {},{}]", c.left, c.top, c.right, c.bottom));
        text.push_str(&format!(
            "  layer {l} z {z} {c:?} {}{} frame {} crop {} transform {:?} blend {:?} alpha {:?} colour {:?}: {buffer}\n",
            if d.composable(c) { "ours" } else { "NOT OURS" },
            if st.hidden.contains(&l) { " HIDDEN" } else { "" },
            frame.unwrap_or_default(),
            crop.unwrap_or_default(),
            d.transform,
            d.blend,
            d.alpha,
            d.color,
        ));
    }
    if text != st.traced {
        eprint!("[composer] layers ({} frame):\n{text}", if st.device_frame { "composer's" } else { "SurfaceFlinger's" });
        st.traced = text;
    }
}

/// Every 600 frames: how many were the composer's own and how many SurfaceFlinger's.
fn log_paths(st: &State) {
    if (st.frames_device + st.frames_client) % 600 == 0 {
        eprintln!("[composer] frames composed here {}, by SurfaceFlinger {}", st.frames_device, st.frames_client);
    }
}

/// A layer's buffer a gralloc handle names.
fn layer_buffer_of(handle: &NativeHandle) -> Option<LayerBuffer> {
    if handle.fds.len() != 1 || handle.ints.len() != HANDLE_INTS || handle.ints[0] as u32 != 0x4247_4d4f {
        return None;
    }
    let Fd(file) = &handle.fds[0];
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return None,
    };
    let (width, height, format, stride, pixels_at) = (handle.ints[2] as u32, handle.ints[3] as u32, handle.ints[5], handle.ints[8] as u32, handle.ints[13] as u32 as u64);
    if stride < width || pixels_at != PIXELS_AT {
        return None;
    }
    shm.as_graphics_buffer();
    let mut name = [0u8; 128];
    let _ = shm.read_at(&mut name, NAME_AT);
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())]).into_owned();
    Some(LayerBuffer { shm, format, stride, width, height, pixels_at, name })
}

/// The client target a gralloc handle names (D2's 14 ints and one region).
fn target_of(handle: &NativeHandle) -> Option<Target> {
    if handle.fds.len() != 1 || handle.ints.len() != HANDLE_INTS || handle.ints[0] as u32 != 0x4247_4d4f {
        return None;
    }
    let Fd(file) = &handle.fds[0];
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return None,
    };
    let (width, height, format, stride, pixels_at) = (handle.ints[2] as u32, handle.ints[3] as u32, handle.ints[5], handle.ints[8] as u32, handle.ints[13] as u32 as u64);
    let target = (width > 0 && height > 0 && stride >= width && pixels_at == PIXELS_AT).then_some(Target { shm, format, width, height, stride, pixels_at });
    if let Some(t) = &target {
        t.shm.as_graphics_buffer();
    }
    target
}

impl IComposerClientServer for Client {
    fn register_callback(&self, _ctx: &Ctx<'_>, callback: Binder) -> Result<(), Status> {
        let Binder::Handle(handle) = callback else { return Err(Status::ServiceSpecific(EX_BAD_LAYER)) };
        self.state.lock().callback = Some(handle);
        // The display is connected from the start: SurfaceFlinger's init needs the hotplug to have
        // arrived by the time registerCallback returns. The call goes to the very thread waiting
        // on this one (the broker routes a host service's call to its caller as nested).
        let _ = IComposerCallbackProxy::new(Arc::clone(&self.broker), handle).on_hotplug(DISPLAY, true);
        Ok(())
    }

    fn create_layer(&self, _ctx: &Ctx<'_>, display: i64, _buffer_slot_count: i32) -> Result<i64, Status> {
        Self::display(display)?;
        let mut st = self.state.lock();
        st.next_layer += 1;
        let id = st.next_layer;
        st.layers.insert(id, Composition::CLIENT);
        Ok(id)
    }

    fn destroy_layer(&self, _ctx: &Ctx<'_>, display: i64, layer: i64) -> Result<(), Status> {
        Self::display(display)?;
        let mut st = self.state.lock();
        st.device.remove(&layer);
        st.layers.remove(&layer).map(|_| ()).ok_or(Status::ServiceSpecific(EX_BAD_LAYER))
    }

    fn execute_commands(&self, _ctx: &Ctx<'_>, commands: Vec<DisplayCommand>) -> Result<Vec<CommandResultPayload>, Status> {
        let mut results = Vec::new();
        for (index, cmd) in commands.into_iter().enumerate() {
            if cmd.display != DISPLAY {
                results.push(CommandResultPayload::Error(CommandError { command_index: index as i32, error_code: EX_BAD_DISPLAY }));
                continue;
            }
            {
                let mut st = self.state.lock();
                for layer in &cmd.layers {
                    if let Some(c) = &layer.composition {
                        st.layers.insert(layer.layer, c.composition);
                    }
                    let d = st.device.entry(layer.layer).or_default();
                    if let Some(buffer) = &layer.buffer {
                        if let Some(b) = buffer.handle.as_ref().and_then(layer_buffer_of) {
                            d.buffers.insert(buffer.slot, b);
                        }
                        d.slot = Some(buffer.slot);
                    }
                    for slot in layer.buffer_slots_to_clear.iter().flatten() {
                        d.buffers.remove(slot);
                    }
                    if let Some(f) = &layer.display_frame {
                        d.frame = Some(f.clone());
                    }
                    if let Some(c) = &layer.source_crop {
                        d.crop = Some(c.clone());
                    }
                    if let Some(b) = &layer.blend_mode {
                        d.blend = Some(b.blend_mode);
                    }
                    if let Some(a) = &layer.plane_alpha {
                        d.alpha = Some(a.alpha);
                    }
                    if let Some(z) = &layer.z {
                        d.z = z.z;
                    }
                    if let Some(t) = &layer.transform {
                        d.transform = Some(t.transform);
                    }
                    if let Some(c) = &layer.color {
                        d.color = Some([c.r, c.g, c.b, c.a]);
                    }
                }
                if let Some(ct) = &cmd.client_target {
                    if let Some(handle) = &ct.buffer.handle {
                        match target_of(handle) {
                            Some(t) => {
                                st.targets.insert(ct.buffer.slot, t);
                            }
                            None => results.push(CommandResultPayload::Error(CommandError { command_index: index as i32, error_code: EX_UNSUPPORTED })),
                        }
                    }
                    st.current_target = Some(ct.buffer.slot);
                }
            }
            if cmd.validate_display || cmd.present_or_validate_display {
                // The composer's own composition when it can take every layer.
                static DEVICE_OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let off = *DEVICE_OFF.get_or_init(|| std::env::var("OMNI_COMPOSER_DEVICE").as_deref() == Ok("0"));
                let hide = !self.show_chrome.load(Ordering::Relaxed);
                let device = {
                    let mut st = self.state.lock();
                    // The chrome left out: layers the composer was asked to draw itself.
                    let hidden: std::collections::HashSet<i64> = if hide {
                        st.layers
                            .iter()
                            .filter(|(l, c)| **c == Composition::DEVICE && st.device.get(l).and_then(|d| d.slot.and_then(|s| d.buffers.get(&s))).is_some_and(|b| is_chrome(&b.name)))
                            .map(|(l, _)| *l)
                            .collect()
                    } else {
                        std::collections::HashSet::new()
                    };
                    let all = !off && !st.layers.is_empty() && st.layers.iter().filter(|(l, _)| !hidden.contains(l)).all(|(l, c)| st.device.get(l).is_some_and(|d| d.composable(*c)));
                    st.hidden = hidden;
                    st.device_frame = all;
                    trace_layers(&mut st);
                    all
                };
                if device {
                    if cmd.present_or_validate_display {
                        results.push(CommandResultPayload::PresentOrValidateResult(PresentOrValidate { display: DISPLAY, result: PresentOrValidate_Result::Validated }));
                    }
                    if cmd.present_display {
                        self.present();
                    }
                    continue;
                }
                // Every layer is composed by the client -- but the chrome left out, which stays the
                // composer's (and is not drawn).
                let changed: Vec<ChangedCompositionLayer> = {
                    let mut st = self.state.lock();
                    let State { layers, hidden, .. } = &mut *st;
                    let mut changed = Vec::new();
                    for (l, c) in layers.iter_mut() {
                        if *c != Composition::CLIENT && !hidden.contains(l) {
                            changed.push(ChangedCompositionLayer { layer: *l, composition: Composition::CLIENT });
                            *c = Composition::CLIENT;
                        }
                    }
                    changed
                };
                if !changed.is_empty() {
                    results.push(CommandResultPayload::ChangedCompositionTypes(ChangedCompositionTypes { display: DISPLAY, layers: changed }));
                }
                if cmd.present_or_validate_display {
                    results.push(CommandResultPayload::PresentOrValidateResult(PresentOrValidate { display: DISPLAY, result: PresentOrValidate_Result::Validated }));
                }
            }
            if cmd.present_display {
                self.present();
            }
        }
        Ok(results)
    }

    fn get_active_config(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        Self::display(display).map(|()| self.mode().config)
    }

    fn get_color_modes(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<ColorMode>, Status> {
        Self::display(display).map(|()| vec![ColorMode::NATIVE])
    }

    fn get_dataspace_saturation_matrix(&self, _ctx: &Ctx<'_>, _dataspace: common::Dataspace) -> Result<Vec<f32>, Status> {
        Ok((0..16).map(|i| if i % 5 == 0 { 1.0 } else { 0.0 }).collect())
    }

    fn get_display_attribute(&self, _ctx: &Ctx<'_>, display: i64, config: i32, attribute: DisplayAttribute) -> Result<i32, Status> {
        Self::display(display)?;
        let mode = self.mode();
        if config != mode.config {
            return Err(Status::ServiceSpecific(1)); // EX_BAD_CONFIG
        }
        Ok(match attribute {
            DisplayAttribute::WIDTH => mode.width as i32,
            DisplayAttribute::HEIGHT => mode.height as i32,
            DisplayAttribute::VSYNC_PERIOD => VSYNC_PERIOD_NS,
            DisplayAttribute::DPI_X | DisplayAttribute::DPI_Y => (DPI * 1000.0) as i32,
            DisplayAttribute::CONFIG_GROUP => 0,
            _ => return Err(Status::ServiceSpecific(4)), // EX_BAD_PARAMETER
        })
    }

    fn get_display_capabilities(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<DisplayCapability>, Status> {
        Self::display(display).map(|()| Vec::new())
    }

    fn get_display_configs(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<i32>, Status> {
        Self::display(display).map(|()| vec![self.mode().config])
    }

    fn get_display_configurations(&self, _ctx: &Ctx<'_>, display: i64, _max_frame_interval_ns: i32) -> Result<Vec<DisplayConfiguration>, Status> {
        Self::display(display)?;
        let mode = self.mode();
        Ok(vec![DisplayConfiguration {
            config_id: mode.config,
            width: mode.width as i32,
            height: mode.height as i32,
            dpi: Some(DisplayConfiguration_Dpi { x: DPI, y: DPI }),
            config_group: 0,
            vsync_period: VSYNC_PERIOD_NS,
            vrr_config: None,
        }])
    }

    fn get_display_connection_type(&self, _ctx: &Ctx<'_>, display: i64) -> Result<DisplayConnectionType, Status> {
        Self::display(display).map(|()| DisplayConnectionType::INTERNAL)
    }

    fn get_display_identification_data(&self, _ctx: &Ctx<'_>, display: i64) -> Result<super::aidl::android_hardware_graphics_composer3::DisplayIdentification, Status> {
        Self::display(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn get_display_name(&self, _ctx: &Ctx<'_>, display: i64) -> Result<String, Status> {
        Self::display(display).map(|()| "omnidroid".to_string())
    }

    fn get_display_vsync_period(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        Self::display(display).map(|()| VSYNC_PERIOD_NS)
    }

    fn get_display_physical_orientation(&self, _ctx: &Ctx<'_>, display: i64) -> Result<common::Transform, Status> {
        Self::display(display).map(|()| common::Transform::NONE)
    }

    fn get_hdr_capabilities(&self, _ctx: &Ctx<'_>, display: i64) -> Result<HdrCapabilities, Status> {
        Self::display(display).map(|()| HdrCapabilities::default())
    }

    fn get_max_virtual_display_count(&self, _ctx: &Ctx<'_>) -> Result<i32, Status> {
        Ok(0)
    }

    fn get_per_frame_metadata_keys(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<PerFrameMetadataKey>, Status> {
        Self::display(display).map(|()| Vec::new())
    }

    fn get_render_intents(&self, _ctx: &Ctx<'_>, display: i64, _mode: ColorMode) -> Result<Vec<RenderIntent>, Status> {
        Self::display(display).map(|()| vec![RenderIntent::COLORIMETRIC])
    }

    fn get_supported_content_types(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<ContentType>, Status> {
        Self::display(display).map(|()| Vec::new())
    }

    fn get_display_decoration_support(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Option<common::DisplayDecorationSupport>, Status> {
        Self::display(display).map(|()| None)
    }

    fn set_active_config(&self, _ctx: &Ctx<'_>, display: i64, config: i32) -> Result<(), Status> {
        Self::display(display)?;
        if config == self.mode().config { Ok(()) } else { Err(Status::ServiceSpecific(1)) }
    }

    /// The one configuration there is, at once: nothing to wait for and no refresh needed.
    fn set_active_config_with_constraints(&self, _ctx: &Ctx<'_>, display: i64, config: i32, _constraints: VsyncPeriodChangeConstraints) -> Result<VsyncPeriodChangeTimeline, Status> {
        Self::display(display)?;
        if config != self.mode().config {
            return Err(Status::ServiceSpecific(1)); // EX_BAD_CONFIG
        }
        let now = crate::sys::monotonic().as_nanos() as i64;
        Ok(VsyncPeriodChangeTimeline { new_vsync_applied_time_nanos: now, refresh_required: false, refresh_time_nanos: 0 })
    }

    fn get_preferred_boot_display_config(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        Self::display(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_auto_low_latency_mode(&self, _ctx: &Ctx<'_>, display: i64, _on: bool) -> Result<(), Status> {
        Self::display(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_client_target_slot_count(&self, _ctx: &Ctx<'_>, display: i64, _count: i32) -> Result<(), Status> {
        Self::display(display)
    }

    fn set_color_mode(&self, _ctx: &Ctx<'_>, display: i64, mode: ColorMode, _intent: RenderIntent) -> Result<(), Status> {
        Self::display(display)?;
        if mode == ColorMode::NATIVE { Ok(()) } else { Err(Status::ServiceSpecific(EX_UNSUPPORTED)) }
    }

    fn set_content_type(&self, _ctx: &Ctx<'_>, display: i64, _type: ContentType) -> Result<(), Status> {
        Self::display(display)
    }

    fn set_power_mode(&self, _ctx: &Ctx<'_>, display: i64, _mode: PowerMode) -> Result<(), Status> {
        Self::display(display)
    }

    fn set_vsync_enabled(&self, _ctx: &Ctx<'_>, display: i64, enabled: bool) -> Result<(), Status> {
        Self::display(display)?;
        self.vsync.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn set_idle_timer_enabled(&self, _ctx: &Ctx<'_>, display: i64, _timeout_ms: i32) -> Result<(), Status> {
        Self::display(display)
    }

    fn get_overlay_support(&self, _ctx: &Ctx<'_>) -> Result<super::aidl::android_hardware_graphics_composer3::OverlayProperties, Status> {
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn get_hdr_conversion_capabilities(&self, _ctx: &Ctx<'_>) -> Result<Vec<common::HdrConversionCapability>, Status> {
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_refresh_rate_changed_callback_debug_enabled(&self, _ctx: &Ctx<'_>, display: i64, _enabled: bool) -> Result<(), Status> {
        Self::display(display)
    }

    fn notify_expected_present(&self, _ctx: &Ctx<'_>, display: i64, _t: ClockMonotonicTimestamp, _frame_interval_ns: i32) -> Result<(), Status> {
        Self::display(display)
    }
}

#[cfg(test)]
mod tests {
    use super::is_chrome;

    /// Names as the image's own windows give them (run 2026-09-28: SystemUI, the launcher, Roblox).
    #[test]
    fn the_bars_and_the_taskbar_are_chrome_and_an_app_is_not() {
        for chrome in ["VRI[StatusBar]#0(BLAST Consumer)0", "VRI[Taskbar]#0(BLAST Consumer)0", "VRI[NavigationBar0]#3(BLAST Consumer)3", "VRI[ScreenDecorOverlayBottom]#5(BLAST Consumer)5"] {
            assert!(is_chrome(chrome), "{chrome}");
        }
        for app in [
            "SurfaceView[com.roblox.client/com.roblox.client.ActivityNativeMain]#2(BLAST Consumer)2",
            "VRI[ActivityNativeMain]#1(BLAST Consumer)1",
            "VRI[FallbackHome]#0(BLAST Consumer)0",
            "bbq-adapter#0(BLAST Consumer)0",
            "",
            "StatusBar",
        ] {
            assert!(!is_chrome(app), "{app}");
        }
    }
}
