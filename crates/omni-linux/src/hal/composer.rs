//! The display's composer, served from the host: `android.hardware.graphics.composer3.IComposer`
//! (AIDL V3), what SurfaceFlinger composes through (D3b design,
//! `docs/superpowers/specs/2026-09-27-d3b-composer-design.md`).
//!
//! One display. The composer composes nothing itself: at validation every layer is changed to
//! `CLIENT` composition, so SurfaceFlinger's RenderEngine composes all of them (on the host GPU,
//! D3a) into the client target, and presenting copies the client target's gralloc region (D2) into
//! the host [`Framebuffer`]. Composition is synchronous, so there are no fences to report.
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
    RenderIntent, ContentType, ClockMonotonicTimestamp,
};
use super::aidl::{Binder, Ctx, Fd, Status};
use super::framebuffer::Framebuffer;
use super::gralloc::PIXELS_AT;
use crate::binder::{Broker, STABILITY_VINTF};
use crate::fd::FileKind;
use crate::shm::Shm;

/// The instance SurfaceFlinger waits for (declared by the image's `hwc3.xml`).
pub const INSTANCE: &str = "android.hardware.graphics.composer3.IComposer/default";

/// The one display's id, size, refresh and density.
const DISPLAY: i64 = 0;
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
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
const IMPLEMENTATION_DEFINED: i32 = 0x22;

pub struct Composer {
    broker: Arc<Broker>,
    framebuffer: Arc<Framebuffer>,
    client: Mutex<Option<Arc<Client>>>,
}

impl Composer {
    #[must_use]
    pub fn new(broker: Arc<Broker>, framebuffer: Arc<Framebuffer>) -> Arc<Self> {
        Arc::new(Self { broker, framebuffer, client: Mutex::new(None) })
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
        let c = Client::new(Arc::clone(&self.broker), Arc::clone(&self.framebuffer));
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
    stride: u32,
    pixels_at: u64,
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
}

pub struct Client {
    broker: Arc<Broker>,
    framebuffer: Arc<Framebuffer>,
    state: Mutex<State>,
    vsync: Arc<AtomicBool>,
}

impl Client {
    fn new(broker: Arc<Broker>, framebuffer: Arc<Framebuffer>) -> Arc<Self> {
        let c = Arc::new(Self { broker, framebuffer, state: Mutex::default(), vsync: Arc::default() });
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

    /// Present the current client target into the framebuffer.
    fn present(&self) {
        let mut st = self.state.lock();
        let Some(target) = st.current_target.and_then(|slot| st.targets.get(&slot)) else { return };
        if !matches!(target.format, RGBA_8888 | RGBX_8888 | IMPLEMENTATION_DEFINED) {
            if st.refused_format != Some(target.format) {
                eprintln!("[composer] a client target of pixel format {:#x} is not presented (RGBA_8888 only)", target.format);
                st.refused_format = Some(target.format);
            }
            return;
        }
        let mut pixels = vec![0u8; target.stride as usize * HEIGHT as usize * 4];
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
            eprintln!("[composer] present slot {:?}: centre {:08x} corner {:08x}", st.current_target, px(WIDTH as usize / 2, HEIGHT as usize / 2), px(8, 8));
        }
        drop(st);
        self.framebuffer.present_rgba(&pixels, stride);
    }
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
    let (format, stride, pixels_at) = (handle.ints[5], handle.ints[8] as u32, handle.ints[13] as u32 as u64);
    (stride >= WIDTH && pixels_at == PIXELS_AT).then_some(Target { shm, format, stride, pixels_at })
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
        self.state.lock().layers.remove(&layer).map(|_| ()).ok_or(Status::ServiceSpecific(EX_BAD_LAYER))
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
                // Every layer is composed by the client.
                let changed: Vec<ChangedCompositionLayer> = {
                    let mut st = self.state.lock();
                    let changed = st.layers.iter().filter(|(_, c)| **c != Composition::CLIENT).map(|(l, _)| ChangedCompositionLayer { layer: *l, composition: Composition::CLIENT }).collect();
                    for c in st.layers.values_mut() {
                        *c = Composition::CLIENT;
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
        Self::display(display).map(|()| 0)
    }

    fn get_color_modes(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<ColorMode>, Status> {
        Self::display(display).map(|()| vec![ColorMode::NATIVE])
    }

    fn get_dataspace_saturation_matrix(&self, _ctx: &Ctx<'_>, _dataspace: common::Dataspace) -> Result<Vec<f32>, Status> {
        Ok((0..16).map(|i| if i % 5 == 0 { 1.0 } else { 0.0 }).collect())
    }

    fn get_display_attribute(&self, _ctx: &Ctx<'_>, display: i64, config: i32, attribute: DisplayAttribute) -> Result<i32, Status> {
        Self::display(display)?;
        if config != 0 {
            return Err(Status::ServiceSpecific(1)); // EX_BAD_CONFIG
        }
        Ok(match attribute {
            DisplayAttribute::WIDTH => WIDTH as i32,
            DisplayAttribute::HEIGHT => HEIGHT as i32,
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
        Self::display(display).map(|()| vec![0])
    }

    fn get_display_configurations(&self, _ctx: &Ctx<'_>, display: i64, _max_frame_interval_ns: i32) -> Result<Vec<DisplayConfiguration>, Status> {
        Self::display(display)?;
        Ok(vec![DisplayConfiguration {
            config_id: 0,
            width: WIDTH as i32,
            height: HEIGHT as i32,
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
        if config == 0 { Ok(()) } else { Err(Status::ServiceSpecific(1)) }
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
