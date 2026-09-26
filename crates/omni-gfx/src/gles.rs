//! [`GfxGlesHost`]: the host's EGL and OpenGL ES, as `omni_android::gles::GlesHost`.
//!
//! # What this is for
//!
//! The engine's renderer choice is its own (D8): it tries Vulkan first and falls back to OpenGL ES
//! 3 when Vulkan does not work -- on a host whose only Vulkan device is a CPU rasteriser it refuses
//! that device by name and asks for EGL. `omni_android::gles` binds the guest's `libEGL.so` and
//! `libGLESv2.so` and translates the calling convention; this file supplies the host functions
//! behind them.
//!
//! # One table per host, chosen by the window at run time
//!
//! There is no `cfg` here (ARCHITECTURE section 2). The [`RawWindow`] behind the guest's window
//! picks a row of [`HOSTS`] -- which libraries to load, which `EGL_PLATFORM_*` to open a display
//! with, how to name the native window -- and everything else is shared:
//!
//! | window | libraries | display | native window |
//! |---|---|---|---|
//! | `RawWindow::Xlib` | `libEGL.so.1`, `libGLESv2.so.2` (GLVND or Mesa) | `eglGetPlatformDisplay(EGL_PLATFORM_X11_KHR, Display *)` | `Window *` for `eglCreatePlatformWindowSurface` |
//! | `RawWindow::Win32` | **not yet plugged in**: ANGLE's `libEGL.dll`, `libGLESv2.dll` | `eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, EGL_DEFAULT_DISPLAY, {EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE, EGL_NONE})` | the `HWND` itself |
//!
//! | `RawWindow::Headless` | the same | `eglGetPlatformDisplay(EGL_PLATFORM_DEVICE_EXT, a device)` -- a hardware device before a software one, unless `LIBGL_ALWAYS_SOFTWARE=1` -- else `EGL_PLATFORM_SURFACELESS_MESA`; `OMNI_EGL_PLATFORM=device\|surfaceless` chooses | none: a **pbuffer** of the headless window's size (`eglCreatePbufferSurface`) |
//!
//! Adding a host is one row in [`HOSTS`] and one arm in [`host_for`] (plus, where the native window
//! is not the X11 shape, one arm in [`native_window`]). Nothing in `omni-android` changes.
//!
//! # Windows: what plugs in (the Windows host's to write)
//!
//! Windows has no system EGL. ANGLE is the ES 3 implementation Chromium, Firefox and Qt ship on
//! Windows, BSD-licensed, and it exposes exactly the EGL + ES surface the guest links against. The
//! row is: libraries `["libEGL.dll"]` and `["libGLESv2.dll"]` (placed beside the executable -- ANGLE
//! is not installed system-wide), display through `eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE
//! = 0x3202, EGL_DEFAULT_DISPLAY, attribs)` with `EGL_PLATFORM_ANGLE_TYPE_ANGLE = 0x3203` set to
//! `EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE = 0x3208`, and the native window for
//! `eglCreateWindowSurface` is the `HWND` (`EGLNativeWindowType` is `HWND` on Windows; with
//! `eglCreatePlatformWindowSurface` it is a pointer to one). The typed callers in
//! `omni_android::gles::signatures` already use the Microsoft x64 convention there -- they are
//! `extern "C"` function-pointer types -- so no calling-convention code is Windows-specific.
//!
//! # macOS: what plugs in (the Mac branch's to write)
//!
//! Apple deprecated OpenGL and has no EGL at all; ANGLE over Metal is the ES 3 implementation
//! (`libEGL.dylib`, `libGLESv2.dylib`, display `EGL_PLATFORM_ANGLE_ANGLE` with
//! `EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE = 0x3489`). Its native window is a **`CAMetalLayer *`**
//! (or an `NSView *` whose layer is one) -- which is why macOS needs a `RawWindow` variant of its own
//! first, carrying the layer; there is none yet, and adding it is the Mac branch's
//! (`omni-platform/src/window/macos.rs`).

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;

use omni_android::gles::{DisplayOpened, GlesHost, HostProc, SurfaceMade};
use omni_android::{AbiError, AbiResult};
use omni_platform::window::RawWindow;

use crate::claim::{claim_window, WindowClaim, WindowKey};

/// `EGL_PLATFORM_X11_KHR` (= `EGL_PLATFORM_X11_EXT`).
pub const EGL_PLATFORM_X11_KHR: u32 = 0x31D5;
/// `EGL_EXTENSIONS`.
const EGL_EXTENSIONS: i32 = 0x3055;
/// `EGL_NONE`.
const EGL_NONE: i32 = 0x3038;
/// `EGL_PLATFORM_DEVICE_EXT`.
pub const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
/// `EGL_PLATFORM_SURFACELESS_MESA`.
pub const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
/// `EGL_WIDTH`, `EGL_HEIGHT`: a pbuffer's size.
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
/// `EGL_GL_COLORSPACE`: the one window-surface attribute a pbuffer takes too.
const EGL_GL_COLORSPACE: i32 = 0x309D;
/// The key [`GfxGlesHost::displays`] keeps the headless display under (no `Display *` is 0).
const HEADLESS_DISPLAY_KEY: usize = 0;

/// Who a GLES window surface's claim names, beside `omni_gfx::Renderer` and the guest's Vulkan
/// swapchain.
pub const GUEST_EGL_SURFACE_OWNER: &str = "the guest's eglCreateWindowSurface";

/// One host's row: what to load and how to open a display for its window system.
#[derive(Debug)]
pub struct HostRow {
    /// `RawWindow::system_name` of the window system this row serves.
    pub system: &'static str,
    /// EGL library names, tried in order.
    pub egl: &'static [&'static str],
    /// GLES library names, tried in order.
    pub gles: &'static [&'static str],
    /// The `EGL_PLATFORM_*` a display is opened with.
    pub platform: u32,
    /// Its name, for the census.
    pub platform_name: &'static str,
    /// The client extensions that platform needs (either one).
    pub platform_extensions: &'static [&'static str],
}

/// Every host this crate can load, one row per window system.
pub static HOSTS: &[HostRow] = &[
    HostRow {
        system: "xlib",
        egl: &["libEGL.so.1", "libEGL.so"],
        gles: &["libGLESv2.so.2", "libGLESv2.so"],
        platform: EGL_PLATFORM_X11_KHR,
        platform_name: "EGL_PLATFORM_X11_KHR",
        platform_extensions: &["EGL_KHR_platform_x11", "EGL_EXT_platform_x11"],
    },
    // No window system: a device's display (NVIDIA, Mesa), or Mesa's surfaceless one.
    HostRow {
        system: "headless",
        egl: &["libEGL.so.1", "libEGL.so"],
        gles: &["libGLESv2.so.2", "libGLESv2.so"],
        platform: EGL_PLATFORM_DEVICE_EXT,
        platform_name: "EGL_PLATFORM_DEVICE_EXT or EGL_PLATFORM_SURFACELESS_MESA",
        platform_extensions: &["EGL_EXT_platform_device", "EGL_MESA_platform_surfaceless"],
    },
];

/// The row for `window`'s system, or the refusal that names what would plug it in.
///
/// # Errors
///
/// Refused for every window system without a row, naming its libraries when they are known.
pub fn host_for(window: RawWindow) -> AbiResult<&'static HostRow> {
    match window {
        RawWindow::Xlib { .. } => Ok(&HOSTS[0]),
        RawWindow::Headless { .. } => Ok(&HOSTS[1]),
        RawWindow::Win32 { .. } => Err(refused(
            "eglGetDisplay",
            "the guest's window is a Win32 window, and this build has no host EGL row for Win32. \
             Windows has no system EGL; what plugs in is ANGLE: load `libEGL.dll` and \
             `libGLESv2.dll`, open the display with eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE \
             0x3202, EGL_DEFAULT_DISPLAY, {EGL_PLATFORM_ANGLE_TYPE_ANGLE 0x3203, \
             EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE 0x3208, EGL_NONE}) and pass the HWND as the \
             native window -- one row in omni_gfx::gles::HOSTS and one arm in host_for. Until \
             then the engine's GLES fallback is refused here by name rather than answered with a \
             display nothing can draw into",
        )),
        other => Err(refused(
            "eglGetDisplay",
            &format!(
                "the guest's window belongs to the `{}` window system, and omni_gfx::gles has no \
                 host EGL row for it (see the module documentation for what each host plugs in)",
                other.system_name()
            ),
        )),
    }
}

fn refused(symbol: &str, why: &str) -> AbiError {
    AbiError::Refused { symbol: symbol.to_string(), address: 0, why: why.to_string() }
}

type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;

struct Loaded {
    row: &'static HostRow,
    egl: libloading::Library,
    gles: libloading::Library,
    get_proc: GetProcAddress,
    description: String,
}

struct Surface {
    display: u64,
    _claim: WindowClaim,
}

type QueryString = unsafe extern "C" fn(*const c_void, i32) -> *const c_char;

/// The host's EGL and GLES, loaded when the guest's first call says which window system it is on.
#[derive(Default)]
pub struct GfxGlesHost {
    loaded: Mutex<Option<Loaded>>,
    displays: Mutex<HashMap<usize, u64>>,
    surfaces: Mutex<HashMap<u64, Surface>>,
}

impl core::fmt::Debug for GfxGlesHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let loaded = self.loaded.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("GfxGlesHost")
            .field("loaded", &loaded.as_ref().map(|l| l.description.clone()))
            .finish_non_exhaustive()
    }
}

fn open_first(names: &'static [&'static str]) -> Result<(libloading::Library, &'static str), String> {
    let mut errors = Vec::new();
    for name in names {
        // SAFETY: loading the host's EGL/GLES runs their initialisers, which is what loading a
        // system graphics library is for; nothing else in this process defines these sonames.
        match unsafe { libloading::Library::new(name) } {
            Ok(library) => return Ok((library, name)),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    Err(errors.join("; "))
}

impl GfxGlesHost {
    /// A host with nothing loaded. Loading happens at [`GlesHost::select`].
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    fn with_loaded<T>(&self, symbol: &str, f: impl FnOnce(&Loaded) -> AbiResult<T>) -> AbiResult<T> {
        let loaded = self.loaded.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match loaded.as_ref() {
            Some(loaded) => f(loaded),
            None => Err(refused(
                symbol,
                "no host EGL is loaded yet: GlesHost::select has not been called with the guest's \
                 window",
            )),
        }
    }

    fn lookup(loaded: &Loaded, name: &str) -> Option<usize> {
        let c_name = CString::new(name).ok()?;
        let library = if name.starts_with("egl") { &loaded.egl } else { &loaded.gles };
        // SAFETY: the symbol is only read as an address here; whoever calls it does so with the
        // registry's prototype for this very name (`HostProc::new`'s contract).
        let exported = unsafe { library.get::<*const c_void>(c_name.as_bytes_with_nul()) }
            .ok()
            .map(|symbol| *symbol as usize)
            .filter(|&address| address != 0);
        exported.or_else(|| {
            // SAFETY: `get_proc` is the host's own eglGetProcAddress, given a NUL-terminated name.
            let address = unsafe { (loaded.get_proc)(c_name.as_ptr()) } as usize;
            (address != 0).then_some(address)
        })
    }

    fn function(&self, symbol: &str, name: &str) -> AbiResult<usize> {
        self.with_loaded(symbol, |loaded| {
            Self::lookup(loaded, name).ok_or_else(|| {
                refused(symbol, &format!("the host EGL ({}) has no `{name}`", loaded.description))
            })
        })
    }

    fn egl_error(&self) -> i32 {
        match self.function("eglGetError", "eglGetError") {
            // SAFETY: the host's `EGLint eglGetError(void)`.
            Ok(address) => unsafe {
                core::mem::transmute::<usize, unsafe extern "C" fn() -> i32>(address)()
            },
            Err(_) => 0,
        }
    }
}

impl GfxGlesHost {
    /// The host's client-extension string (`eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS)`).
    fn client_extensions(&self) -> AbiResult<String> {
        let query = self.function("eglGetDisplay", "eglQueryString")?;
        // SAFETY: the host's `const char *eglQueryString(EGLDisplay, EGLint)`; EGL_NO_DISPLAY asks
        // for the client extensions (EGL_EXT_client_extensions).
        let text = unsafe { core::mem::transmute::<usize, QueryString>(query)(core::ptr::null(), EGL_EXTENSIONS) };
        Ok(if text.is_null() {
            String::new()
        } else {
            // SAFETY: a non-NULL result is a NUL-terminated string the host EGL owns.
            unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
        })
    }

    /// **The display for a headless window** (`RawWindow::Headless`): one that needs no window
    /// system. `EGL_PLATFORM_DEVICE_EXT` over the first hardware device -- the GPU, NVIDIA's or
    /// Mesa's, with no X server -- or the first software one when `LIBGL_ALWAYS_SOFTWARE=1` asks
    /// for the CPU; else `EGL_PLATFORM_SURFACELESS_MESA`. `OMNI_EGL_PLATFORM=device|surfaceless`
    /// picks one.
    fn headless_display(&self) -> AbiResult<DisplayOpened> {
        const CALL: &str = "eglGetDisplay";
        if let Some(&display) =
            self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&HEADLESS_DISPLAY_KEY)
        {
            return Ok(DisplayOpened { display, host_call: "the headless display, again".to_string() });
        }
        let client = self.client_extensions()?;
        let has = |name: &str| client.split_whitespace().any(|c| c == name);
        let (entry, spelled) = match self.function(CALL, "eglGetPlatformDisplay") {
            Ok(address) => (address, "eglGetPlatformDisplay"),
            Err(_) => (self.function(CALL, "eglGetPlatformDisplayEXT")?, "eglGetPlatformDisplayEXT"),
        };
        // SAFETY: `EGLDisplay eglGetPlatformDisplay[EXT](EGLenum, void *, const EGLAttrib/EGLint *)`
        // with a NULL list, which both forms accept.
        let get = |platform: u32, native: *mut c_void| unsafe {
            core::mem::transmute::<usize, unsafe extern "C" fn(u32, *mut c_void, *const c_void) -> *mut c_void>(
                entry,
            )(platform, native, core::ptr::null())
        } as u64;
        let asked = std::env::var("OMNI_EGL_PLATFORM").ok();
        let software = std::env::var("LIBGL_ALWAYS_SOFTWARE").is_ok_and(|v| v.trim() == "1");
        let mut tried = Vec::new();
        let order: &[&str] = match asked.as_deref() {
            Some("surfaceless") => &["surfaceless"],
            Some("device") => &["device"],
            _ => &["device", "surfaceless"],
        };
        for platform in order {
            match *platform {
                "device" if has("EGL_EXT_platform_device") => match self.pick_device(software) {
                    Ok((device, described)) => {
                        let display = get(EGL_PLATFORM_DEVICE_EXT, device as *mut c_void);
                        if display != 0 {
                            return Ok(self.keep_headless(
                                display,
                                format!("{spelled}(EGL_PLATFORM_DEVICE_EXT, {described})"),
                            ));
                        }
                        tried.push(format!("the device {described}: EGL_NO_DISPLAY, eglGetError {:#x}", self.egl_error()));
                    }
                    Err(why) => tried.push(why),
                },
                "surfaceless" if has("EGL_MESA_platform_surfaceless") => {
                    let display = get(EGL_PLATFORM_SURFACELESS_MESA, core::ptr::null_mut());
                    if display != 0 {
                        return Ok(self.keep_headless(
                            display,
                            format!("{spelled}(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY)"),
                        ));
                    }
                    tried.push(format!("surfaceless: EGL_NO_DISPLAY, eglGetError {:#x}", self.egl_error()));
                }
                other => tried.push(format!("{other}: the host EGL does not have that platform")),
            }
        }
        Err(refused(
            CALL,
            &format!(
                "the guest's window is headless and no host EGL display without a window system                  could be opened ({}); client extensions: \"{client}\"",
                tried.join("; ")
            ),
        ))
    }

    fn keep_headless(&self, display: u64, host_call: String) -> DisplayOpened {
        self.displays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(HEADLESS_DISPLAY_KEY, display);
        eprintln!("GLES: the headless display is {host_call} = {display:#x}");
        DisplayOpened { display, host_call }
    }

    /// The `EGLDeviceEXT` a headless display opens: the first hardware device (no
    /// `EGL_MESA_device_software`), or the first software one when `software`; else the first.
    fn pick_device(&self, software: bool) -> Result<(usize, String), String> {
        let query_devices = self
            .function("eglGetDisplay", "eglQueryDevicesEXT")
            .map_err(|_| "device: the host EGL has no eglQueryDevicesEXT".to_string())?;
        let query_string = self.function("eglGetDisplay", "eglQueryDeviceStringEXT").ok();
        let mut devices = [0usize; 16];
        let mut count: i32 = 0;
        // SAFETY: `EGLBoolean eglQueryDevicesEXT(EGLint max, EGLDeviceEXT *devices, EGLint *num)`,
        // with room for `max` devices.
        let ok = unsafe {
            core::mem::transmute::<usize, unsafe extern "C" fn(i32, *mut usize, *mut i32) -> u32>(query_devices)(
                devices.len() as i32,
                devices.as_mut_ptr(),
                &mut count,
            )
        };
        let count = if ok == 0 { 0 } else { count.clamp(0, devices.len() as i32) as usize };
        if count == 0 {
            return Err(format!("device: eglQueryDevicesEXT found none (eglGetError {:#x})", self.egl_error()));
        }
        let described: Vec<(usize, String, bool)> = devices[..count]
            .iter()
            .enumerate()
            .map(|(index, &device)| {
                let extensions = query_string.map_or_else(String::new, |query| {
                    // SAFETY: `const char *eglQueryDeviceStringEXT(EGLDeviceEXT, EGLint)` of a
                    // device the host just enumerated; a non-NULL answer is a C string it owns.
                    let text = unsafe {
                        core::mem::transmute::<usize, unsafe extern "C" fn(usize, i32) -> *const c_char>(query)(
                            device,
                            EGL_EXTENSIONS,
                        )
                    };
                    if text.is_null() {
                        String::new()
                    } else {
                        // SAFETY: as above.
                        unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
                    }
                });
                let is_software = extensions.split_whitespace().any(|e| e == "EGL_MESA_device_software");
                (index, extensions, is_software)
            })
            .collect();
        let chosen = described
            .iter()
            .find(|(_, _, is_software)| *is_software == software)
            .or_else(|| described.first())
            .expect("at least one device");
        Ok((
            devices[chosen.0],
            format!(
                "device {} of {count} ({}: {})",
                chosen.0,
                if chosen.2 { "software" } else { "hardware" },
                chosen.1
            ),
        ))
    }

    /// A headless window's surface: a pbuffer of its size.
    fn create_pbuffer_for(
        &self,
        display: u64,
        config: u64,
        id: u64,
        width: u32,
        height: u32,
        attributes: &[i32],
    ) -> AbiResult<SurfaceMade> {
        const CALL: &str = "eglCreateWindowSurface";
        let claim = claim_window(WindowKey::headless(id), GUEST_EGL_SURFACE_OWNER).map_err(|held| {
            refused(CALL, &format!("the headless window {id} is already owned by {}", held.owner))
        })?;
        let mut list = vec![EGL_WIDTH, width as i32, EGL_HEIGHT, height as i32];
        // The guest's colour space is kept; the rest of a window surface's attributes
        // (`EGL_RENDER_BUFFER`, ...) are not a pbuffer's.
        for pair in attributes.chunks_exact(2) {
            if pair[0] == EGL_GL_COLORSPACE {
                list.extend_from_slice(pair);
            }
        }
        list.push(EGL_NONE);
        let entry = self.function(CALL, "eglCreatePbufferSurface")?;
        // SAFETY: `EGLSurface eglCreatePbufferSurface(EGLDisplay, EGLConfig, const EGLint *)`, with
        // an EGL_NONE-terminated list that outlives the call.
        let surface = unsafe {
            core::mem::transmute::<usize, unsafe extern "C" fn(u64, u64, *const i32) -> u64>(entry)(
                display,
                config,
                list.as_ptr(),
            )
        };
        let host_call = format!("eglCreatePbufferSurface({width}x{height}) for the headless window");
        if surface != 0 {
            self.surfaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(surface, Surface { display, _claim: claim });
        } else {
            eprintln!("GLES: {host_call} failed, eglGetError {:#x}", self.egl_error());
        }
        Ok(SurfaceMade { surface, host_call })
    }
}

impl GlesHost for GfxGlesHost {
    fn select(&self, window: RawWindow) -> AbiResult<String> {
        let row = host_for(window)?;
        let mut loaded = self.loaded.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = loaded.as_ref() {
            if existing.row.system == row.system {
                return Ok(existing.description.clone());
            }
            return Err(refused(
                "eglGetDisplay",
                &format!(
                    "this host already loaded {} for the {} window system, and the guest's window \
                     is now a {} window; one guest gets one set of GL entry points",
                    existing.description, existing.row.system, row.system
                ),
            ));
        }
        let (egl, egl_name) = open_first(row.egl).map_err(|why| {
            refused("eglGetDisplay", &format!("no host EGL library could be loaded ({why})"))
        })?;
        let (gles, gles_name) = open_first(row.gles).map_err(|why| {
            refused("eglGetDisplay", &format!("no host GLES library could be loaded ({why})"))
        })?;
        // SAFETY: `eglGetProcAddress` has this prototype in every EGL (EGL 1.5 section 3.10).
        let get_proc: GetProcAddress = unsafe { egl.get::<GetProcAddress>(b"eglGetProcAddress\0") }
            .map(|symbol| *symbol)
            .map_err(|error| {
                refused("eglGetDisplay", &format!("{egl_name} exports no eglGetProcAddress: {error}"))
            })?;
        let description = format!("{egl_name} + {gles_name} over {}", row.platform_name);
        *loaded = Some(Loaded { row, egl, gles, get_proc, description: description.clone() });
        Ok(description)
    }

    fn proc_address(&self, name: &str) -> AbiResult<Option<HostProc>> {
        self.with_loaded(name, |loaded| {
            // SAFETY: the address is the host's function for `name`, from its own library or its
            // own eglGetProcAddress, and the libraries are held for this host's life.
            Ok(Self::lookup(loaded, name).and_then(|address| unsafe { HostProc::new(address) }))
        })
    }

    fn default_display(&self, window: RawWindow) -> AbiResult<DisplayOpened> {
        if let RawWindow::Headless { .. } = window {
            return self.headless_display();
        }
        let RawWindow::Xlib { display: native, .. } = window else {
            host_for(window)?;
            return Err(refused("eglGetDisplay", "no display for this window system"));
        };
        if let Some(&display) =
            self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&native)
        {
            return Ok(DisplayOpened {
                display,
                host_call: format!("eglGetPlatformDisplay(EGL_PLATFORM_X11_KHR, Display* {native:#x})"),
            });
        }
        let (row, client) = self.with_loaded("eglGetDisplay", |loaded| {
            let query = Self::lookup(loaded, "eglQueryString")
                .ok_or_else(|| refused("eglGetDisplay", "the host EGL has no eglQueryString"))?;
            // SAFETY: the host's `const char *eglQueryString(EGLDisplay, EGLint)`; EGL_NO_DISPLAY
            // asks for the client extensions (EGL_EXT_client_extensions).
            let text = unsafe {
                core::mem::transmute::<usize, unsafe extern "C" fn(*const c_void, i32) -> *const c_char>(
                    query,
                )(core::ptr::null(), EGL_EXTENSIONS)
            };
            let client = if text.is_null() {
                String::new()
            } else {
                // SAFETY: a non-NULL result is a NUL-terminated string the host EGL owns.
                unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
            };
            Ok((loaded.row, client))
        })?;
        if !row.platform_extensions.iter().any(|e| client.split_whitespace().any(|c| c == *e)) {
            return Err(refused(
                "eglGetDisplay",
                &format!(
                    "the host EGL's client extensions do not include {:?}, so it cannot open a \
                     display for {}: \"{client}\"",
                    row.platform_extensions, row.platform_name
                ),
            ));
        }
        // EGL 1.5's eglGetPlatformDisplay takes EGLAttrib; the EXT form takes EGLint. Both accept
        // a NULL list.
        let (entry, spelled) = match self.function("eglGetDisplay", "eglGetPlatformDisplay") {
            Ok(address) => (address, "eglGetPlatformDisplay"),
            Err(_) => (self.function("eglGetDisplay", "eglGetPlatformDisplayEXT")?, "eglGetPlatformDisplayEXT"),
        };
        // SAFETY: `EGLDisplay eglGetPlatformDisplay[EXT](EGLenum, void *, const EGLAttrib/EGLint *)`;
        // `native` is the live `Display *` of the guest's window (XInitThreads has run: see
        // omni-platform's window/linux.rs), and the list is NULL.
        let display = unsafe {
            core::mem::transmute::<usize, unsafe extern "C" fn(u32, *mut c_void, *const c_void) -> *mut c_void>(
                entry,
            )(row.platform, native as *mut c_void, core::ptr::null())
        } as u64;
        if display == 0 {
            return Err(refused(
                "eglGetDisplay",
                &format!(
                    "the host's {spelled}({}, Display* {native:#x}) answered EGL_NO_DISPLAY, \
                     eglGetError {:#x}",
                    row.platform_name,
                    self.egl_error()
                ),
            ));
        }
        self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(native, display);
        Ok(DisplayOpened {
            display,
            host_call: format!("{spelled}({}, Display* {native:#x})", row.platform_name),
        })
    }

    fn create_window_surface(
        &self,
        display: u64,
        config: u64,
        window: RawWindow,
        attributes: &[i32],
    ) -> AbiResult<SurfaceMade> {
        const CALL: &str = "eglCreateWindowSurface";
        if let RawWindow::Headless { id, width, height } = window {
            return self.create_pbuffer_for(display, config, id, width, height, attributes);
        }
        let (key, native) = native_window(window)?;
        let claim = claim_window(key, GUEST_EGL_SURFACE_OWNER).map_err(|held| {
            refused(
                CALL,
                &format!(
                    "the window {:#x} is already owned by {}: a native window takes one \
                     presenter, and a second one over it would fight the first for its pixels",
                    held.window.raw(),
                    held.owner
                ),
            )
        })?;
        let mut terminated: Vec<i32> = attributes.to_vec();
        if terminated.last() != Some(&EGL_NONE) {
            terminated.push(EGL_NONE);
        }
        // EGL 1.5's form takes EGLAttrib (pointer-sized); widen each value, keep EGL_NONE.
        let widened: Vec<isize> = terminated.iter().map(|&v| v as isize).collect();
        let (surface, spelled) = match self.function(CALL, "eglCreatePlatformWindowSurface") {
            Ok(entry) => {
                // SAFETY: `EGLSurface eglCreatePlatformWindowSurface(EGLDisplay, EGLConfig,
                // void *native_window, const EGLAttrib *)`; for X11 `native_window` points at the
                // `Window` (EGL_KHR_platform_x11), which outlives the call.
                let surface = unsafe {
                    core::mem::transmute::<
                        usize,
                        unsafe extern "C" fn(u64, u64, *const c_void, *const isize) -> u64,
                    >(entry)(display, config, (&raw const native).cast(), widened.as_ptr())
                };
                (surface, "eglCreatePlatformWindowSurface")
            }
            Err(_) => {
                let entry = self.function(CALL, "eglCreatePlatformWindowSurfaceEXT")?;
                // SAFETY: the EXT form, which takes an `EGLint` list.
                let surface = unsafe {
                    core::mem::transmute::<
                        usize,
                        unsafe extern "C" fn(u64, u64, *const c_void, *const i32) -> u64,
                    >(entry)(display, config, (&raw const native).cast(), terminated.as_ptr())
                };
                (surface, "eglCreatePlatformWindowSurfaceEXT")
            }
        };
        let host_call = format!("{spelled}(X11 Window {native:#x})");
        if surface != 0 {
            self.surfaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(surface, Surface { display, _claim: claim });
        }
        Ok(SurfaceMade { surface, host_call })
    }

    fn destroy_window_surface(&self, display: u64, surface: u64) -> AbiResult<u32> {
        const CALL: &str = "eglDestroySurface";
        let entry = self.function(CALL, "eglDestroySurface")?;
        let held = self.surfaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&surface);
        let Some(held) = held else {
            return Err(refused(CALL, &format!("the surface {surface:#x} is not one this host made")));
        };
        // SAFETY: `EGLBoolean eglDestroySurface(EGLDisplay, EGLSurface)`.
        let r = unsafe { core::mem::transmute::<usize, unsafe extern "C" fn(u64, u64) -> u32>(entry)(display, surface) };
        if r == 0 {
            self.surfaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(surface, held);
        }
        Ok(r)
    }

    fn display_terminated(&self, display: u64) {
        self.surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, surface| surface.display != display);
    }
}

/// The claim key and the native-window value for `window`.
///
/// # Errors
///
/// Refused for a window system without a row.
pub fn native_window(window: RawWindow) -> AbiResult<(WindowKey, u64)> {
    match window {
        RawWindow::Xlib { window, .. } => Ok((WindowKey::xlib(window), window)),
        other => {
            host_for(other)?;
            Err(refused("eglCreateWindowSurface", "no native window for this window system"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The Windows door is a refusal naming what plugs in**, not a silent NULL display.
    #[test]
    fn a_win32_window_is_refused_naming_angle() {
        let error = host_for(RawWindow::Win32 { hwnd: 1, hinstance: 2 }).expect_err("no Win32 row yet");
        let text = error.to_string();
        for needle in ["libEGL.dll", "libGLESv2.dll", "EGL_PLATFORM_ANGLE_ANGLE", "D3D11", "HWND"] {
            assert!(text.contains(needle), "{needle} missing from: {text}");
        }
        let host = GfxGlesHost::new();
        assert!(host.select(RawWindow::Win32 { hwnd: 1, hinstance: 2 }).is_err());
        assert!(host.proc_address("glClear").is_err(), "nothing is loaded after a refused select");
    }

    #[test]
    fn an_xlib_window_picks_the_x11_row() {
        let row = host_for(RawWindow::Xlib { display: 1, window: 2 }).expect("an X11 row");
        assert_eq!(row.platform, 0x31D5);
        assert_eq!(row.egl[0], "libEGL.so.1");
        assert_eq!(row.gles[0], "libGLESv2.so.2");
    }
}
