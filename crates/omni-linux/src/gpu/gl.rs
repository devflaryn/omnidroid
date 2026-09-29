//! The GL fallback's host half: `ioctl(/dev/omni-gpu, OMNI_GL_CALL)` from the guest's GLES driver
//! (`device/src/gl/`, `/vendor/lib64/egl/libGLES_omni.so`), answered on the host's EGL and GLES.
//!
//! **GLES commands** are called as they are. Guest and host share one address space, so a pointer
//! argument is the host's pointer too and the host's driver reads and writes the guest's memory where
//! it lies -- client-side vertex arrays included, which it reads at draw time. What differs is the
//! calling convention, and that is the Khronos registries': the command table and one typed caller
//! per calling shape are `omni-android`'s generated `gles/signatures.rs`, included here by path so
//! both GLES layers call through the same table (and the guest driver, generated from it by
//! `tools/gen_gl_forward.py`, sends its fingerprint at `eglInitialize`).
//!
//! **What cannot be called as it is** ([`special`] ids): EGL, which is the guest driver's over this
//! file's contexts and pbuffers; strings and buffer maps, which would hand the guest host memory
//! (strings are copied into the guest's buffer; a map is a guest shadow copied in at map and out at
//! unmap or flush); and gralloc buffers -- a window surface is a host pbuffer whose frame
//! `eglSwapBuffers` reads back into the window's buffer, and an `EGLImage` of a buffer is a host
//! texture holding its pixels (uploaded when the guest targets it, read back into the buffer at
//! flush points once it has been a framebuffer attachment).
//!
//! **Which host EGL** ([`ROWS`]): the first row whose libraries load. Only the Linux row has run
//! (Mesa on a Quadro 4000 through nouveau, and llvmpipe); the ANGLE rows are what Windows (D3D11)
//! and macOS (Metal) plug in, each with its own display, and are not claimed until run there.
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::errno::{SysResult, EINVAL};
use crate::process::{Process, Task};

/// The guest-to-host command table: every GLES command's guest and host calling shape.
#[path = "../../../omni-android/src/gles/signatures.rs"]
#[allow(dead_code, clippy::all, clippy::pedantic)]
mod signatures;

use signatures::{Signature, SHAPES, SIGNATURES};

/// `_IOWR('G', 2, struct omni_gpu_call)`: the Vulkan driver's call, numbered for GL.
pub const OMNI_GL_CALL: u64 = 0xc020_4702;

/// The driver's own requests (`device/src/gl/gl.h`), above every GL command's id.
pub mod special {
    pub const HELLO: u32 = 0x10000;
    pub const CONTEXT_CREATE: u32 = 0x10001;
    pub const CONTEXT_DESTROY: u32 = 0x10002;
    pub const SURFACE_CREATE: u32 = 0x10003;
    pub const SURFACE_DESTROY: u32 = 0x10004;
    pub const MAKE_CURRENT: u32 = 0x10005;
    pub const SWAP: u32 = 0x10006;
    pub const SURFACE_RESIZE: u32 = 0x10007;
    pub const GET_STRING: u32 = 0x10008;
    pub const MAP: u32 = 0x10009;
    pub const UNMAP: u32 = 0x1000a;
    pub const FLUSH_MAPPED: u32 = 0x1000b;
    pub const BUFFER_POINTER: u32 = 0x1000c;
    pub const IMAGE_TARGET: u32 = 0x1000d;
    pub const IMAGE_DESTROY: u32 = 0x1000e;
    pub const FLUSH_IMAGES: u32 = 0x1000f;
}

/// A host answer that is an EGL error.
const ERROR_BIT: u64 = 1 << 63;

/// `egl.c`'s descriptor of its RGBA8888 config with a 24-bit depth and 8-bit stencil buffer:
/// r | g << 8 | b << 16 | a << 24 | depth << 32 | stencil << 40.
const RGBA8_D24S8: u64 = 8 | 8 << 8 | 8 << 16 | 8 << 24 | 24 << 32 | 8 << 40;

// EGL.
const EGL_NONE: i32 = 0x3038;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_ALPHA_SIZE: i32 = 0x3021;
const EGL_DEPTH_SIZE: i32 = 0x3025;
const EGL_STENCIL_SIZE: i32 = 0x3026;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x1;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x4;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;
const EGL_GL_TEXTURE_LEVEL_KHR: i32 = 0x30BC;
const EGL_DRAW: i32 = 0x3059;
const EGL_READ: i32 = 0x305A;
const EGL_BAD_ALLOC: u64 = 0x3003;
const EGL_BAD_CONFIG: u64 = 0x3005;
const EGL_BAD_CONTEXT: u64 = 0x3006;
const EGL_BAD_MATCH: u64 = 0x3009;
const EGL_BAD_PARAMETER: u64 = 0x300C;
const EGL_BAD_SURFACE: u64 = 0x300D;
const EGL_NOT_INITIALIZED: u64 = 0x3001;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const EGL_PLATFORM_ANGLE_ANGLE: u32 = 0x3202;
const EGL_PLATFORM_ANGLE_TYPE_ANGLE: isize = 0x3203;
const EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE: isize = 0x3208;
const EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE: isize = 0x3489;

// GLES.
const GL_VERSION: u32 = 0x1F02;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_NUM_EXTENSIONS: u32 = 0x821D;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_BINDING_2D: u32 = 0x8069;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
const GL_TEXTURE_BINDING_EXTERNAL_OES: u32 = 0x8D67;
const GL_RENDERBUFFER_BINDING: u32 = 0x8CA7;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_RGBA: u32 = 0x1908;
const GL_RGB: u32 = 0x1907;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_UNSIGNED_SHORT_5_6_5: u32 = 0x8363;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_PACK_ROW_LENGTH: u32 = 0x0D02;
const GL_PACK_SKIP_ROWS: u32 = 0x0D03;
const GL_PACK_SKIP_PIXELS: u32 = 0x0D04;
const GL_UNPACK_ROW_LENGTH: u32 = 0x0CF2;
const GL_UNPACK_SKIP_ROWS: u32 = 0x0CF3;
const GL_UNPACK_SKIP_PIXELS: u32 = 0x0CF4;
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
const GL_PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;
const GL_PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x88EF;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_LINEAR: i32 = 0x2601;
const GL_MAP_READ_BIT: u64 = 0x1;
const GL_MAP_WRITE_BIT: u64 = 0x2;
const GL_MAP_FLUSH_EXPLICIT_BIT: u64 = 0x10;

// HAL pixel formats.
const HAL_RGBA_8888: u32 = 1;
const HAL_RGBX_8888: u32 = 2;
const HAL_RGB_565: u32 = 4;
const HAL_BGRA_8888: u32 = 5;

/// Extensions the host has and the guest is not offered, each for what forwarding cannot give:
/// a persistent coherent map cannot be shadowed (as the old path's GLES layer withholds it); the
/// others import host objects (an EGLImage as storage, memory objects, semaphores) by means the
/// guest does not have.
const WITHHELD: &[&str] = &[
    "GL_EXT_buffer_storage",
    "GL_EXT_EGL_image_storage",
    "GL_EXT_EGL_image_storage_compression",
    "GL_EXT_memory_object",
    "GL_EXT_memory_object_fd",
    "GL_EXT_semaphore",
    "GL_EXT_semaphore_fd",
    "GL_EXT_external_buffer",
];

// --- the command table ----------------------------------------------------------------------

/// The `gl*` rows of the table, in order: a command's id is its index here.
fn commands() -> &'static [&'static Signature] {
    static GL: OnceLock<Vec<&'static Signature>> = OnceLock::new();
    GL.get_or_init(|| SIGNATURES.iter().filter(|s| s.name.starts_with("gl")).collect())
}

/// The table's fingerprint, as `tools/gen_gl_forward.py` computes it: FNV-1a 64 over
/// `name:params:ret;` of every `gl*` command, in order.
#[must_use]
pub fn fingerprint() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in commands() {
        for b in format!("{}:{}:{};", s.name, s.params, s.ret as char).bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The id of GL command `name`.
#[must_use]
pub fn command_id(name: &str) -> Option<u32> {
    commands().iter().position(|s| s.name == name).map(|i| i as u32)
}

// --- the host's EGL and GLES ----------------------------------------------------------------

/// How a row opens its display.
#[derive(Debug, Clone, Copy)]
enum DisplayKind {
    /// Mesa or NVIDIA: `EGL_PLATFORM_DEVICE_EXT` over the first hardware device (a software one when
    /// `LIBGL_ALWAYS_SOFTWARE=1`), else `EGL_PLATFORM_SURFACELESS_MESA`. No window system is needed:
    /// every surface here is a pbuffer. `OMNI_EGL_PLATFORM=device|surfaceless` picks one.
    DeviceOrSurfaceless,
    /// ANGLE: `eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, EGL_DEFAULT_DISPLAY, {TYPE, t})`.
    Angle(isize),
}

/// One host's EGL: its libraries and its display.
#[derive(Debug)]
pub struct Row {
    pub name: &'static str,
    egl: &'static [&'static str],
    gles: &'static [&'static str],
    display: DisplayKind,
}

/// Every host EGL this file can open, tried in order: the first whose libraries load.
pub static ROWS: &[Row] = &[
    Row { name: "Mesa/NVIDIA EGL (Linux)", egl: &["libEGL.so.1", "libEGL.so"], gles: &["libGLESv2.so.2", "libGLESv2.so"], display: DisplayKind::DeviceOrSurfaceless },
    // Not yet run: Windows' (ANGLE beside the executable, over Direct3D 11).
    Row { name: "ANGLE on Direct3D 11 (Windows)", egl: &["libEGL.dll"], gles: &["libGLESv2.dll"], display: DisplayKind::Angle(EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE) },
    // Not yet run: macOS' (ANGLE over Metal; Apple has no EGL).
    Row { name: "ANGLE on Metal (macOS)", egl: &["libEGL.dylib"], gles: &["libGLESv2.dylib"], display: DisplayKind::Angle(EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE) },
];

type GetProc = unsafe extern "C" fn(*const c_char) -> *const c_void;

macro_rules! egl_fns {
    ($($field:ident: $name:literal => fn($($arg:ty),*) -> $ret:ty;)*) => {
        #[allow(non_snake_case)]
        struct Egl { $($field: unsafe extern "C" fn($($arg),*) -> $ret,)* }
        impl Egl {
            fn load(lib: &libloading::Library, get_proc: GetProc) -> Result<Self, String> {
                Ok(Self { $($field: {
                    let address = lookup(lib, get_proc, $name).ok_or_else(|| format!("the host EGL has no {}", $name))?;
                    // SAFETY: the host's entry point of that name, whose prototype is EGL's.
                    unsafe { std::mem::transmute::<usize, unsafe extern "C" fn($($arg),*) -> $ret>(address) }
                },)* })
            }
        }
    };
}

egl_fns! {
    get_error: "eglGetError" => fn() -> i32;
    query_string: "eglQueryString" => fn(usize, i32) -> *const c_char;
    initialize: "eglInitialize" => fn(usize, *mut i32, *mut i32) -> u32;
    bind_api: "eglBindAPI" => fn(u32) -> u32;
    choose_config: "eglChooseConfig" => fn(usize, *const i32, *mut usize, i32, *mut i32) -> u32;
    get_config_attrib: "eglGetConfigAttrib" => fn(usize, usize, i32, *mut i32) -> u32;
    create_context: "eglCreateContext" => fn(usize, usize, usize, *const i32) -> usize;
    destroy_context: "eglDestroyContext" => fn(usize, usize) -> u32;
    create_pbuffer: "eglCreatePbufferSurface" => fn(usize, usize, *const i32) -> usize;
    destroy_surface: "eglDestroySurface" => fn(usize, usize) -> u32;
    make_current: "eglMakeCurrent" => fn(usize, usize, usize, usize) -> u32;
    current_context: "eglGetCurrentContext" => fn() -> usize;
    current_surface: "eglGetCurrentSurface" => fn(i32) -> usize;
    create_image: "eglCreateImageKHR" => fn(usize, usize, u32, usize, *const i32) -> usize;
    destroy_image: "eglDestroyImageKHR" => fn(usize, usize) -> u32;
}

/// An entry point by name: exported by `lib`, else the host's `eglGetProcAddress`.
fn lookup(lib: &libloading::Library, get_proc: GetProc, name: &str) -> Option<usize> {
    let c = CString::new(name).ok()?;
    // SAFETY: read as an address only.
    let exported = unsafe { lib.get::<*const c_void>(c.as_bytes_with_nul()) }.ok().map(|s| *s as usize).filter(|&a| a != 0);
    // SAFETY: the host's eglGetProcAddress, given a NUL-terminated name.
    exported.or_else(|| Some(unsafe { get_proc(c.as_ptr()) } as usize).filter(|&a| a != 0))
}

/// The host's EGL and GLES, opened once per host process.
struct Host {
    /// Kept loaded for the process's life.
    _egl_lib: libloading::Library,
    gles_lib: libloading::Library,
    get_proc: GetProc,
    egl: Egl,
    display: usize,
    described: String,
    /// `EGL_KHR_no_config_context`: contexts are made without a config and fit every surface.
    no_config: bool,
    /// `EGL_KHR_surfaceless_context`.
    surfaceless: bool,
    /// The host's GLES version (major, minor) and its extensions less [`WITHHELD`].
    version: (u32, u32),
    extensions: Vec<String>,
    /// Each command's host address: 0 not yet looked up, 1 the host has none.
    fns: Vec<AtomicUsize>,
    configs: Mutex<HashMap<u64, usize>>,
}

// SAFETY: the libraries and the display are process-wide host objects; EGL is thread-safe.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

fn host() -> Result<&'static Host, String> {
    static HOST: OnceLock<Result<Host, String>> = OnceLock::new();
    HOST.get_or_init(open_host).as_ref().map_err(Clone::clone)
}

/// Whether this host has a GLES the fallback can open: its description, or why not.
pub fn host_available() -> Result<String, String> {
    host().map(|h| h.described.clone())
}

fn open_first(names: &[&str]) -> Result<libloading::Library, String> {
    let mut why = Vec::new();
    for name in names {
        // SAFETY: loading the host's EGL/GLES runs their initialisers, which is what loading a
        // system graphics library is for.
        match unsafe { libloading::Library::new(name) } {
            Ok(l) => return Ok(l),
            Err(e) => why.push(format!("{name}: {e}")),
        }
    }
    Err(why.join("; "))
}

fn open_host() -> Result<Host, String> {
    let mut why = Vec::new();
    for row in ROWS {
        match open_row(row) {
            Ok(h) => {
                eprintln!("[gl] host GLES: {} (pid {})", h.described, std::process::id());
                return Ok(h);
            }
            Err(e) => why.push(format!("{}: {e}", row.name)),
        }
    }
    Err(why.join(" | "))
}

fn c_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: a non-NULL answer of the host's EGL/GL is a NUL-terminated string it owns.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn open_row(row: &'static Row) -> Result<Host, String> {
    let egl_lib = open_first(row.egl)?;
    let gles_lib = open_first(row.gles)?;
    // SAFETY: read as an address; called with eglGetProcAddress's prototype.
    let get_proc: GetProc = unsafe {
        *egl_lib.get::<GetProc>(b"eglGetProcAddress\0").map_err(|e| format!("eglGetProcAddress: {e}"))?
    };
    let egl = Egl::load(&egl_lib, get_proc)?;
    let (display, how) = open_display(&egl_lib, get_proc, &egl, row.display)?;
    let (mut major, mut minor) = (0, 0);
    // SAFETY: a display the host just opened.
    if unsafe { (egl.initialize)(display, &mut major, &mut minor) } == 0 {
        return Err(format!("eglInitialize({how}) failed, eglGetError {:#x}", unsafe { (egl.get_error)() }));
    }
    // SAFETY: the host's EGL, initialised.
    unsafe { (egl.bind_api)(EGL_OPENGL_ES_API) };
    let exts = c_string(unsafe { (egl.query_string)(display, EGL_EXTENSIONS) });
    let has = |e: &str| exts.split_whitespace().any(|x| x == e);
    let mut host = Host {
        _egl_lib: egl_lib,
        gles_lib,
        get_proc,
        egl,
        display,
        described: String::new(),
        no_config: has("EGL_KHR_no_config_context"),
        surfaceless: has("EGL_KHR_surfaceless_context"),
        version: (0, 0),
        extensions: Vec::new(),
        fns: (0..commands().len()).map(|_| AtomicUsize::new(0)).collect(),
        configs: Mutex::new(HashMap::new()),
    };
    let (version, renderer, extensions) = host.probe()?;
    host.version = version;
    host.extensions = extensions.split_whitespace().filter(|e| !WITHHELD.contains(e)).map(String::from).collect();
    host.described = format!("{} -- {how}, EGL {major}.{minor}, GLES {}.{} on {renderer}", row.name, version.0, version.1);
    Ok(host)
}

fn open_display(lib: &libloading::Library, get_proc: GetProc, egl: &Egl, kind: DisplayKind) -> Result<(usize, String), String> {
    let get_platform = lookup(lib, get_proc, "eglGetPlatformDisplay").or_else(|| lookup(lib, get_proc, "eglGetPlatformDisplayEXT")).ok_or("the host EGL has no eglGetPlatformDisplay")?;
    match kind {
        DisplayKind::Angle(t) => {
            let attribs: [isize; 3] = [EGL_PLATFORM_ANGLE_TYPE_ANGLE, t, EGL_NONE as isize];
            // SAFETY: `EGLDisplay eglGetPlatformDisplay(EGLenum, void *, const EGLAttrib *)`.
            let d = unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(u32, usize, *const isize) -> usize>(get_platform)(EGL_PLATFORM_ANGLE_ANGLE, 0, attribs.as_ptr()) };
            if d == 0 {
                return Err(format!("eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, {t:#x}): EGL_NO_DISPLAY"));
            }
            Ok((d, format!("ANGLE display type {t:#x}")))
        }
        DisplayKind::DeviceOrSurfaceless => {
            let client = c_string(unsafe { (egl.query_string)(0, EGL_EXTENSIONS) });
            let has = |e: &str| client.split_whitespace().any(|x| x == e);
            // SAFETY: as above, with a NULL attribute list, which both spellings accept.
            let get = |platform: u32, native: usize| unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(u32, usize, *const c_void) -> usize>(get_platform)(platform, native, std::ptr::null()) };
            let software = std::env::var("LIBGL_ALWAYS_SOFTWARE").is_ok_and(|v| v.trim() == "1");
            let order: &[&str] = match std::env::var("OMNI_EGL_PLATFORM").as_deref() {
                Ok("surfaceless") => &["surfaceless"],
                Ok("device") => &["device"],
                _ => &["device", "surfaceless"],
            };
            let mut tried = Vec::new();
            for platform in order {
                match *platform {
                    "device" if has("EGL_EXT_platform_device") => match pick_device(lib, get_proc, software) {
                        Ok((device, described)) => {
                            let d = get(EGL_PLATFORM_DEVICE_EXT, device);
                            if d != 0 {
                                return Ok((d, format!("EGL_PLATFORM_DEVICE_EXT {described}")));
                            }
                            tried.push(format!("device {described}: EGL_NO_DISPLAY"));
                        }
                        Err(e) => tried.push(e),
                    },
                    "surfaceless" if has("EGL_MESA_platform_surfaceless") => {
                        let d = get(EGL_PLATFORM_SURFACELESS_MESA, 0);
                        if d != 0 {
                            return Ok((d, "EGL_PLATFORM_SURFACELESS_MESA".to_string()));
                        }
                        tried.push("surfaceless: EGL_NO_DISPLAY".to_string());
                    }
                    other => tried.push(format!("{other}: not offered")),
                }
            }
            Err(format!("no display without a window system ({}); client extensions \"{client}\"", tried.join("; ")))
        }
    }
}

/// The `EGLDeviceEXT` to open: the first hardware one (or software one when asked), else the first.
fn pick_device(lib: &libloading::Library, get_proc: GetProc, software: bool) -> Result<(usize, String), String> {
    let query = lookup(lib, get_proc, "eglQueryDevicesEXT").ok_or("device: no eglQueryDevicesEXT")?;
    let query_string = lookup(lib, get_proc, "eglQueryDeviceStringEXT");
    let mut devices = [0usize; 16];
    let mut count = 0i32;
    // SAFETY: `EGLBoolean eglQueryDevicesEXT(EGLint, EGLDeviceEXT *, EGLint *)` with room for 16.
    let ok = unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(i32, *mut usize, *mut i32) -> u32>(query)(16, devices.as_mut_ptr(), &mut count) };
    let count = if ok == 0 { 0 } else { count.clamp(0, 16) as usize };
    if count == 0 {
        return Err("device: none".into());
    }
    let is_software = |d: usize| {
        query_string.is_some_and(|q| {
            // SAFETY: `const char *eglQueryDeviceStringEXT(EGLDeviceEXT, EGLint)` of an enumerated device.
            let s = c_string(unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(usize, i32) -> *const c_char>(q)(d, EGL_EXTENSIONS) });
            s.split_whitespace().any(|e| e == "EGL_MESA_device_software")
        })
    };
    let chosen = (0..count).find(|&i| is_software(devices[i]) == software).unwrap_or(0);
    Ok((devices[chosen], format!("device {chosen} of {count} ({})", if is_software(devices[chosen]) { "software" } else { "hardware" })))
}

impl Host {
    /// The host address of command `id`, looked up once.
    fn function(&self, id: u32) -> Option<usize> {
        let slot = self.fns.get(id as usize)?;
        match slot.load(Ordering::Relaxed) {
            0 => {
                let name = commands()[id as usize].name;
                let found = lookup(&self.gles_lib, self.get_proc, name);
                slot.store(found.unwrap_or(1), Ordering::Relaxed);
                if found.is_none() {
                    eprintln!("[gl] the host GLES has no {name}");
                }
                found
            }
            1 => None,
            f => Some(f),
        }
    }

    /// Call command `name` with `args` (their bit patterns, in the guest's classes).
    fn call(&self, name: &str, args: &[u64]) -> u64 {
        let Some(id) = command_id(name) else { return 0 };
        self.call_id(id, args)
    }

    fn call_id(&self, id: u32, args: &[u64]) -> u64 {
        let Some(sig) = commands().get(id as usize) else { return 0 };
        let Some(f) = self.function(id) else { return 0 };
        if args.len() != sig.abi.len() {
            return 0;
        }
        // SAFETY: `f` is the host's `sig.name`, whose C prototype is this shape (the table's).
        unsafe { (SHAPES[sig.shape as usize].call)(f, args) }
    }

    fn get_integer(&self, pname: u32) -> i32 {
        let mut v: i32 = 0;
        self.call("glGetIntegerv", &[u64::from(pname), &mut v as *mut i32 as u64]);
        v
    }

    fn error(&self) -> u64 {
        // SAFETY: the host's eglGetError.
        ERROR_BIT | u64::from(unsafe { (self.egl.get_error)() } as u32 & 0xffff)
    }

    /// A host config for a guest descriptor (`egl.c`'s), cached.
    fn config(&self, descriptor: u64) -> Option<usize> {
        if let Some(&c) = self.configs.lock().get(&descriptor) {
            return Some(c);
        }
        let want = |shift: u32| ((descriptor >> shift) & 0xff) as i32;
        let (r, g, b, a, depth, stencil) = (want(0), want(8), want(16), want(24), want(32), want(40));
        let attribs = [
            EGL_RED_SIZE, r, EGL_GREEN_SIZE, g, EGL_BLUE_SIZE, b, EGL_ALPHA_SIZE, a, EGL_DEPTH_SIZE, depth, EGL_STENCIL_SIZE, stencil,
            EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE,
        ];
        let mut configs = [0usize; 64];
        let mut n = 0i32;
        // SAFETY: an EGL_NONE-terminated list and room for 64 configs.
        if unsafe { (self.egl.choose_config)(self.display, attribs.as_ptr(), configs.as_mut_ptr(), 64, &mut n) } == 0 || n <= 0 {
            return None;
        }
        let attr = |c: usize, a: i32| {
            let mut v = 0;
            // SAFETY: a config the host just chose.
            unsafe { (self.egl.get_config_attrib)(self.display, c, a, &mut v) };
            v
        };
        // The exact colour sizes first (the host sorts deeper configs ahead), else the host's first.
        let chosen = configs[..n as usize]
            .iter()
            .copied()
            .find(|&c| attr(c, EGL_RED_SIZE) == r && attr(c, EGL_GREEN_SIZE) == g && attr(c, EGL_BLUE_SIZE) == b && attr(c, EGL_ALPHA_SIZE) == a)
            .unwrap_or(configs[0]);
        self.configs.lock().insert(descriptor, chosen);
        Some(chosen)
    }

    /// The host's GLES version, renderer and extensions, from a context made for the question.
    fn probe(&self) -> Result<((u32, u32), String, String), String> {
        // SAFETY: the host's EGL, initialised; what this thread had current is restored below.
        let (was_ctx, was_draw, was_read) = unsafe { ((self.egl.current_context)(), (self.egl.current_surface)(EGL_DRAW), (self.egl.current_surface)(EGL_READ)) };
        let config = if self.no_config { 0 } else { self.config(RGBA8_D24S8).ok_or("no host config")? };
        let attribs = [EGL_CONTEXT_MAJOR_VERSION, 2, EGL_NONE];
        // SAFETY: as above.
        let ctx = unsafe { (self.egl.create_context)(self.display, config, 0, attribs.as_ptr()) };
        if ctx == 0 {
            return Err(format!("eglCreateContext(ES 2) failed, eglGetError {:#x}", unsafe { (self.egl.get_error)() }));
        }
        let pbuffer = if self.surfaceless {
            0
        } else {
            let c = self.config(RGBA8_D24S8).ok_or("no host config")?;
            let a = [EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE];
            // SAFETY: as above.
            unsafe { (self.egl.create_pbuffer)(self.display, c, a.as_ptr()) }
        };
        // SAFETY: as above.
        if unsafe { (self.egl.make_current)(self.display, pbuffer, pbuffer, ctx) } == 0 {
            return Err(format!("eglMakeCurrent failed, eglGetError {:#x}", unsafe { (self.egl.get_error)() }));
        }
        let string = |name: u32| c_string(self.call("glGetString", &[u64::from(name)]) as *const c_char);
        let version_text = string(GL_VERSION);
        let renderer = string(0x1F01);
        let extensions = string(GL_EXTENSIONS);
        // SAFETY: as above.
        unsafe {
            (self.egl.make_current)(self.display, was_draw, was_read, was_ctx);
            (self.egl.destroy_context)(self.display, ctx);
            if pbuffer != 0 {
                (self.egl.destroy_surface)(self.display, pbuffer);
            }
        }
        let version = parse_es_version(&version_text).ok_or_else(|| format!("GL_VERSION \"{version_text}\" is not an OpenGL ES version"))?;
        Ok((version, renderer, extensions))
    }
}

/// `"OpenGL ES 3.1 Mesa 26.0.8"` -> (3, 1).
#[must_use]
pub fn parse_es_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.strip_prefix("OpenGL ES ")?;
    let rest = rest.strip_prefix("-CM ").unwrap_or(rest);
    let number = rest.split_whitespace().next()?;
    let (major, minor) = number.split_once('.')?;
    Some((major.parse().ok()?, minor.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()?))
}

// --- objects --------------------------------------------------------------------------------

/// A pbuffer: a window surface's back buffer, or a guest pbuffer.
struct Surface {
    pbuffer: usize,
    config: usize,
    width: u32,
    height: u32,
}

/// An `EGLImage`'s host side in one context: a texture holding the buffer's pixels, and the host
/// image made of it that the guest's textures and renderbuffers are targeted with.
struct Image {
    texture: u32,
    host_image: usize,
    shm: Arc<crate::shm::Shm>,
    stride: u32,
    pixels_at: u64,
    width: u32,
    height: u32,
    format: u32,
    /// The region's content generation the texture holds.
    generation: u64,
    /// Attached to a framebuffer: read back into the buffer at flush points.
    rendered: bool,
}

#[derive(Default)]
struct ContextInfo {
    /// The context is ES 3 or later (known at its first make-current).
    es3: Option<bool>,
    /// The framebuffer images are read back through.
    readback_fbo: u32,
    /// By the guest's image.
    images: HashMap<u64, Image>,
    /// The guest's texture (0) and renderbuffer (1) names targeted with an image.
    names: HashMap<(u8, u32), u64>,
}

struct Mapping {
    host: usize,
    shadow: u64,
    length: usize,
    access: u64,
}

#[derive(Default)]
struct Current {
    context: usize,
    draw: u64,
    read: u64,
    /// Buffer maps, by target.
    maps: HashMap<u32, Mapping>,
}

thread_local! {
    static CURRENT: RefCell<Current> = RefCell::new(Current::default());
}

struct Objects {
    surfaces: Mutex<HashMap<u64, Surface>>,
    contexts: Mutex<HashMap<usize, Arc<Mutex<ContextInfo>>>>,
    next_surface: AtomicU64,
}

fn objects() -> &'static Objects {
    static O: OnceLock<Objects> = OnceLock::new();
    O.get_or_init(|| Objects { surfaces: Mutex::new(HashMap::new()), contexts: Mutex::new(HashMap::new()), next_surface: AtomicU64::new(1) })
}

fn context_info(ctx: usize) -> Option<Arc<Mutex<ContextInfo>>> {
    objects().contexts.lock().get(&ctx).cloned()
}

fn current_context() -> usize {
    CURRENT.with(|c| c.borrow().context)
}

// --- the ioctl ------------------------------------------------------------------------------

/// `ioctl(OMNI_GL_CALL)` on `/dev/omni-gpu`.
pub fn ioctl(p: &Process, t: &mut Task, arg: u64) -> SysResult {
    let call = p.mem.read(arg, 32)?;
    let id = u32::from_le_bytes(call[0..4].try_into().expect("4"));
    let argc = u32::from_le_bytes(call[4..8].try_into().expect("4"));
    let args_at = u64::from_le_bytes(call[8..16].try_into().expect("8"));
    if argc > 32 {
        return Err(EINVAL);
    }
    let args: Vec<u64> = if argc == 0 {
        Vec::new()
    } else {
        p.mem.read(args_at, argc as usize * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8"))).collect()
    };
    static TRACE: OnceLock<bool> = OnceLock::new();
    let trace = *TRACE.get_or_init(|| std::env::var("OMNI_GL_TRACE").as_deref() == Ok("1"));
    let result = if id >= special::HELLO {
        special_call(p, id, &args)
    } else {
        match host() {
            Ok(h) => command(h, p, id, &args),
            Err(_) => 0,
        }
    };
    if trace {
        let name = commands().get(id as usize).map_or_else(|| format!("special {id:#x}"), |s| s.name.to_string());
        eprintln!("[gl] {}:{} {name} {args:x?} -> {result:#x}", p.sys.pid, t.tid);
    }
    p.mem.write(arg + 16, &result.to_le_bytes())?;
    Ok(0)
}

/// Command ids the host watches, looked up once.
struct Watched {
    get_integerv: u32,
    finish: u32,
    flush: u32,
    fence_sync: u32,
    framebuffer_texture_2d: u32,
    framebuffer_renderbuffer: u32,
    delete_textures: u32,
    delete_renderbuffers: u32,
}

fn watched() -> &'static Watched {
    static W: OnceLock<Watched> = OnceLock::new();
    W.get_or_init(|| {
        let id = |n: &str| command_id(n).unwrap_or(u32::MAX);
        Watched {
            get_integerv: id("glGetIntegerv"),
            finish: id("glFinish"),
            flush: id("glFlush"),
            fence_sync: id("glFenceSync"),
            framebuffer_texture_2d: id("glFramebufferTexture2D"),
            framebuffer_renderbuffer: id("glFramebufferRenderbuffer"),
            delete_textures: id("glDeleteTextures"),
            delete_renderbuffers: id("glDeleteRenderbuffers"),
        }
    })
}

/// A GL command, as it is, with what the host keeps in step around the few it watches.
fn command(h: &Host, p: &Process, id: u32, args: &[u64]) -> u64 {
    let w = watched();
    if id == w.fence_sync {
        // What a fence orders includes the rendered images reaching their buffers.
        flush_images(h);
    }
    let r = h.call_id(id, args);
    if id == w.finish || id == w.flush {
        flush_images(h);
    } else if id == w.get_integerv && args.len() == 2 && args[0] as u32 == GL_NUM_EXTENSIONS {
        // The count of the extensions offered, not the host's.
        let _ = p.mem.write(args[1], &(h.extensions.len() as i32).to_le_bytes());
    } else if (id == w.framebuffer_texture_2d || id == w.framebuffer_renderbuffer) && args.len() >= 4 {
        let kind = u8::from(id == w.framebuffer_renderbuffer);
        let name = args[3] as u32;
        if let Some(info) = context_info(current_context()) {
            let mut info = info.lock();
            if let Some(&image) = info.names.get(&(kind, name)) {
                if let Some(img) = info.images.get_mut(&image) {
                    img.rendered = true;
                }
            }
        }
    } else if (id == w.delete_textures || id == w.delete_renderbuffers) && args.len() >= 2 {
        let kind = u8::from(id == w.delete_renderbuffers);
        let n = (args[0] as u32).min(4096) as usize;
        if let (Ok(bytes), Some(info)) = (p.mem.read(args[1], n * 4), context_info(current_context())) {
            let mut info = info.lock();
            for name in bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().expect("4"))) {
                info.names.remove(&(kind, name));
            }
        }
    }
    r
}

fn special_call(p: &Process, id: u32, a: &[u64]) -> u64 {
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    if id == special::HELLO {
        if arg(0) != fingerprint() {
            eprintln!("[gl] the guest's GLES driver was built from another command table ({:#x}, the host's {:#x}): rebuild libGLES_omni.so (tools/gen_gl_forward.py)", arg(0), fingerprint());
            return ERROR_BIT | EGL_NOT_INITIALIZED;
        }
        return match host() {
            Ok(h) => u64::from(h.version.0 << 8 | h.version.1),
            Err(e) => {
                eprintln!("[gl] no host GLES: {e}");
                ERROR_BIT | EGL_NOT_INITIALIZED
            }
        };
    }
    let Ok(h) = host() else { return ERROR_BIT | EGL_NOT_INITIALIZED };
    match id {
        special::CONTEXT_CREATE => context_create(h, arg(0) as usize, arg(1) as i32, arg(2) as i32),
        special::CONTEXT_DESTROY => {
            let ctx = arg(0) as usize;
            if objects().contexts.lock().remove(&ctx).is_some() {
                // SAFETY: a context this file made.
                unsafe { (h.egl.destroy_context)(h.display, ctx) };
            }
            1
        }
        special::SURFACE_CREATE => surface_create(h, arg(0), arg(1) as u32, arg(2) as u32),
        special::SURFACE_DESTROY => {
            if let Some(s) = objects().surfaces.lock().remove(&arg(0)) {
                // SAFETY: a pbuffer this file made (EGL defers it while it is current).
                unsafe { (h.egl.destroy_surface)(h.display, s.pbuffer) };
            }
            1
        }
        special::MAKE_CURRENT => make_current(h, arg(0), arg(1), arg(2) as usize),
        special::SWAP => swap(h, p, arg(0), arg(1), arg(2) as u32, arg(3) as u32, arg(4) as u32),
        special::SURFACE_RESIZE => surface_resize(h, arg(0), arg(1) as u32, arg(2) as u32),
        special::GET_STRING => get_string(h, p, arg(0) as u32, arg(1) as u32, arg(2), arg(3)),
        special::MAP => map(h, p, arg(0) as u32, arg(1), arg(2), arg(3), arg(4)),
        special::UNMAP => unmap(h, p, arg(0) as u32),
        special::FLUSH_MAPPED => {
            flush_mapped(h, p, arg(0) as u32, arg(1), arg(2));
            0
        }
        special::BUFFER_POINTER => CURRENT.with(|c| c.borrow().maps.get(&(arg(0) as u32)).map_or(0, |m| m.shadow)),
        special::IMAGE_TARGET => {
            image_target(h, p, arg(0), arg(1) as u32, arg(2), arg(3), arg(4) as u32, arg(5) as u32, arg(6) as u32);
            0
        }
        special::IMAGE_DESTROY => {
            image_destroy(h, arg(0));
            0
        }
        special::FLUSH_IMAGES => {
            flush_images(h);
            0
        }
        _ => ERROR_BIT | EGL_BAD_PARAMETER,
    }
}

fn context_create(h: &Host, share: usize, major: i32, minor: i32) -> u64 {
    if share != 0 && !objects().contexts.lock().contains_key(&share) {
        return ERROR_BIT | EGL_BAD_CONTEXT;
    }
    let config = if h.no_config {
        0
    } else {
        match h.config(RGBA8_D24S8) {
            Some(c) => c,
            None => return ERROR_BIT | EGL_BAD_CONFIG,
        }
    };
    let attribs = [EGL_CONTEXT_MAJOR_VERSION, major, EGL_CONTEXT_MINOR_VERSION, minor, EGL_NONE];
    // SAFETY: the host's display, an EGL_NONE-terminated list, a context this file made or none.
    let ctx = unsafe { (h.egl.create_context)(h.display, config, share, attribs.as_ptr()) };
    if ctx == 0 {
        return h.error();
    }
    objects().contexts.lock().insert(ctx, Arc::new(Mutex::new(ContextInfo::default())));
    ctx as u64
}

fn surface_create(h: &Host, descriptor: u64, width: u32, height: u32) -> u64 {
    let Some(config) = h.config(descriptor) else { return ERROR_BIT | EGL_BAD_CONFIG };
    let pbuffer = match make_pbuffer(h, config, width, height) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let id = objects().next_surface.fetch_add(1, Ordering::Relaxed);
    objects().surfaces.lock().insert(id, Surface { pbuffer, config, width: width.max(1), height: height.max(1) });
    id
}

fn make_pbuffer(h: &Host, config: usize, width: u32, height: u32) -> Result<usize, u64> {
    let attribs = [EGL_WIDTH, width.max(1) as i32, EGL_HEIGHT, height.max(1) as i32, EGL_NONE];
    // SAFETY: a host config and an EGL_NONE-terminated list.
    let pbuffer = unsafe { (h.egl.create_pbuffer)(h.display, config, attribs.as_ptr()) };
    if pbuffer == 0 {
        let e = h.error();
        return Err(if e == ERROR_BIT { ERROR_BIT | EGL_BAD_ALLOC } else { e });
    }
    Ok(pbuffer)
}

fn make_current(h: &Host, draw: u64, read: u64, ctx: usize) -> u64 {
    let (d, r) = {
        let surfaces = objects().surfaces.lock();
        let find = |id: u64| if id == 0 { Some(0) } else { surfaces.get(&id).map(|s| s.pbuffer) };
        match (find(draw), find(read)) {
            (Some(d), Some(r)) => (d, r),
            _ => return ERROR_BIT | EGL_BAD_SURFACE,
        }
    };
    if ctx != 0 && !objects().contexts.lock().contains_key(&ctx) {
        return ERROR_BIT | EGL_BAD_CONTEXT;
    }
    // SAFETY: host objects this file made, or none.
    if unsafe { (h.egl.make_current)(h.display, d, r, ctx) } == 0 {
        return h.error();
    }
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        c.context = ctx;
        c.draw = draw;
        c.read = read;
    });
    if ctx != 0 {
        if let Some(info) = context_info(ctx) {
            let mut info = info.lock();
            if info.es3.is_none() {
                let text = c_string(h.call("glGetString", &[u64::from(GL_VERSION)]) as *const c_char);
                info.es3 = Some(parse_es_version(&text).is_some_and(|v| v.0 >= 3));
            }
        }
    }
    1
}

fn surface_resize(h: &Host, id: u64, width: u32, height: u32) -> u64 {
    let config = match objects().surfaces.lock().get(&id) {
        Some(s) => s.config,
        None => return ERROR_BIT | EGL_BAD_SURFACE,
    };
    let pbuffer = match make_pbuffer(h, config, width, height) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old = {
        let mut surfaces = objects().surfaces.lock();
        let Some(s) = surfaces.get_mut(&id) else { return ERROR_BIT | EGL_BAD_SURFACE };
        let old = s.pbuffer;
        s.pbuffer = pbuffer;
        s.width = width.max(1);
        s.height = height.max(1);
        old
    };
    // Current here: drawn into from now on.
    let (ctx, draw, read) = CURRENT.with(|c| {
        let c = c.borrow();
        (c.context, c.draw, c.read)
    });
    if ctx != 0 && (draw == id || read == id) {
        let surfaces = objects().surfaces.lock();
        let pb = |s: u64| surfaces.get(&s).map_or(0, |s| s.pbuffer);
        // SAFETY: host objects this file made.
        unsafe { (h.egl.make_current)(h.display, pb(draw), pb(read), ctx) };
    }
    // SAFETY: the pbuffer this surface had (EGL defers it while another thread has it current).
    unsafe { (h.egl.destroy_surface)(h.display, old) };
    1
}

// --- pixels ---------------------------------------------------------------------------------

/// Bytes per pixel of a gralloc format this driver's surfaces and images have.
fn bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        HAL_RGBA_8888 | HAL_RGBX_8888 | HAL_BGRA_8888 => Some(4),
        HAL_RGB_565 => Some(2),
        _ => None,
    }
}

/// One RGBA8 row into `format`'s bytes.
fn pack_row(rgba: &[u8], format: u32, out: &mut [u8]) {
    match format {
        HAL_BGRA_8888 => {
            for (s, d) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        }
        HAL_RGB_565 => {
            for (s, d) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(2)) {
                let v = (u16::from(s[0]) >> 3) << 11 | (u16::from(s[1]) >> 2) << 5 | u16::from(s[2]) >> 3;
                d.copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => out[..rgba.len()].copy_from_slice(rgba),
    }
}

/// `rows` RGBA8 rows of `width` pixels (bottom-up when `flip`) into `format` rows at `stride`
/// pixels, top-down.
#[must_use]
pub fn to_buffer(rgba: &[u8], width: usize, rows: usize, flip: bool, format: u32, stride: usize) -> Option<Vec<u8>> {
    let bpp = bytes_per_pixel(format)?;
    let mut out = vec![0u8; stride * rows * bpp];
    for r in 0..rows {
        let src_row = if flip { rows - 1 - r } else { r };
        let src = rgba.get(src_row * width * 4..(src_row + 1) * width * 4)?;
        pack_row(src, format, &mut out[r * stride * bpp..(r * stride + width) * bpp]);
    }
    Some(out)
}

/// A buffer's rows (`format`, `stride` pixels) as tightly packed pixels for `glTexImage2D`, and
/// that upload's format and type.
#[must_use]
pub fn from_buffer(bytes: &[u8], width: usize, rows: usize, format: u32, stride: usize) -> Option<(Vec<u8>, u32, u32)> {
    let bpp = bytes_per_pixel(format)?;
    let mut out = Vec::with_capacity(width * rows * bpp);
    for r in 0..rows {
        let row = bytes.get(r * stride * bpp..(r * stride + width) * bpp)?;
        match format {
            HAL_BGRA_8888 => row.chunks_exact(4).for_each(|p| out.extend_from_slice(&[p[2], p[1], p[0], p[3]])),
            HAL_RGBX_8888 => row.chunks_exact(4).for_each(|p| out.extend_from_slice(&[p[0], p[1], p[2], 255])),
            _ => out.extend_from_slice(row),
        }
    }
    let (f, t) = if format == HAL_RGB_565 { (GL_RGB, GL_UNSIGNED_SHORT_5_6_5) } else { (GL_RGBA, GL_UNSIGNED_BYTE) };
    Some((out, f, t))
}

/// Pack state saved around a read of the host's own.
struct PackState {
    pack_buffer: i32,
    alignment: i32,
    row_length: i32,
    skip_rows: i32,
    skip_pixels: i32,
}

fn save_pack(h: &Host, es3: bool) -> PackState {
    let s = PackState {
        pack_buffer: if es3 { h.get_integer(GL_PIXEL_PACK_BUFFER_BINDING) } else { 0 },
        alignment: h.get_integer(GL_PACK_ALIGNMENT),
        row_length: if es3 { h.get_integer(GL_PACK_ROW_LENGTH) } else { 0 },
        skip_rows: if es3 { h.get_integer(GL_PACK_SKIP_ROWS) } else { 0 },
        skip_pixels: if es3 { h.get_integer(GL_PACK_SKIP_PIXELS) } else { 0 },
    };
    if es3 {
        h.call("glBindBuffer", &[u64::from(GL_PIXEL_PACK_BUFFER), 0]);
        h.call("glPixelStorei", &[u64::from(GL_PACK_ROW_LENGTH), 0]);
        h.call("glPixelStorei", &[u64::from(GL_PACK_SKIP_ROWS), 0]);
        h.call("glPixelStorei", &[u64::from(GL_PACK_SKIP_PIXELS), 0]);
    }
    h.call("glPixelStorei", &[u64::from(GL_PACK_ALIGNMENT), 4]);
    s
}

fn restore_pack(h: &Host, es3: bool, s: &PackState) {
    h.call("glPixelStorei", &[u64::from(GL_PACK_ALIGNMENT), s.alignment as u64]);
    if es3 {
        h.call("glPixelStorei", &[u64::from(GL_PACK_ROW_LENGTH), s.row_length as u64]);
        h.call("glPixelStorei", &[u64::from(GL_PACK_SKIP_ROWS), s.skip_rows as u64]);
        h.call("glPixelStorei", &[u64::from(GL_PACK_SKIP_PIXELS), s.skip_pixels as u64]);
        h.call("glBindBuffer", &[u64::from(GL_PIXEL_PACK_BUFFER), s.pack_buffer as u64]);
    }
}

fn read_pixels(h: &Host, x: i32, y: i32, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    h.call("glReadPixels", &[x as u64, y as u64, u64::from(width), u64::from(height), u64::from(GL_RGBA), u64::from(GL_UNSIGNED_BYTE), rgba.as_mut_ptr() as u64]);
    rgba
}

fn es3_of(ctx: usize) -> bool {
    context_info(ctx).and_then(|i| i.lock().es3).unwrap_or(false)
}

/// `eglSwapBuffers`: the surface's frame into the window's buffer.
fn swap(h: &Host, p: &Process, id: u64, handle: u64, width: u32, height: u32, format: u32) -> u64 {
    let ctx = current_context();
    let (draw, read) = CURRENT.with(|c| (c.borrow().draw, c.borrow().read));
    if ctx == 0 || draw != id {
        return ERROR_BIT | EGL_BAD_SURFACE;
    }
    flush_images(h);
    let (surface_w, surface_h) = match objects().surfaces.lock().get(&id) {
        Some(s) => (s.width, s.height),
        None => return ERROR_BIT | EGL_BAD_SURFACE,
    };
    let Ok((shm, stride, pixels_at)) = super::native::gralloc_buffer(p, handle) else { return ERROR_BIT | EGL_BAD_MATCH };
    if bytes_per_pixel(format).is_none() {
        return ERROR_BIT | EGL_BAD_MATCH;
    }
    let (w, rows) = (width.min(surface_w), height.min(surface_h));
    let es3 = es3_of(ctx);
    // Framebuffer 0 of the draw surface: read it as the read surface when they differ.
    let other_read = read != draw;
    if other_read {
        let pb = objects().surfaces.lock().get(&draw).map_or(0, |s| s.pbuffer);
        // SAFETY: host objects this file made.
        unsafe { (h.egl.make_current)(h.display, pb, pb, ctx) };
    }
    let binding = if es3 { GL_READ_FRAMEBUFFER_BINDING } else { GL_FRAMEBUFFER_BINDING };
    let target = if es3 { GL_READ_FRAMEBUFFER } else { GL_FRAMEBUFFER };
    let was_fbo = h.get_integer(binding);
    h.call("glBindFramebuffer", &[u64::from(target), 0]);
    let pack = save_pack(h, es3);
    // The window's top rows are the surface's top rows: GL counts from the bottom.
    let rgba = read_pixels(h, 0, (surface_h - rows) as i32, w, rows);
    restore_pack(h, es3, &pack);
    h.call("glBindFramebuffer", &[u64::from(target), was_fbo as u64]);
    if other_read {
        let surfaces = objects().surfaces.lock();
        let pb = |s: u64| surfaces.get(&s).map_or(0, |s| s.pbuffer);
        // SAFETY: as above.
        unsafe { (h.egl.make_current)(h.display, pb(draw), pb(read), ctx) };
    }
    let Some(bytes) = to_buffer(&rgba, w as usize, rows as usize, true, format, stride as usize) else { return ERROR_BIT | EGL_BAD_MATCH };
    let _ = shm.write_at(&bytes, pixels_at);
    super::native::bump_generation(&shm);
    1
}

// --- strings --------------------------------------------------------------------------------

fn get_string(h: &Host, p: &Process, name: u32, index: u32, buffer: u64, capacity: u64) -> u64 {
    let text = if name == GL_EXTENSIONS {
        if index == u32::MAX {
            h.extensions.join(" ")
        } else {
            match h.extensions.get(index as usize) {
                Some(e) => e.clone(),
                None => return 0,
            }
        }
    } else {
        if current_context() == 0 {
            return 0;
        }
        let r = if index == u32::MAX { h.call("glGetString", &[u64::from(name)]) } else { h.call("glGetStringi", &[u64::from(name), u64::from(index)]) };
        if r == 0 {
            return 0;
        }
        c_string(r as *const c_char)
    };
    let need = text.len() as u64 + 1;
    if buffer != 0 && capacity >= need {
        let mut bytes = text.into_bytes();
        bytes.push(0);
        if p.mem.write(buffer, &bytes).is_err() {
            return 0;
        }
    }
    need
}

// --- buffer maps ----------------------------------------------------------------------------

fn map(h: &Host, p: &Process, target: u32, offset: u64, length: u64, access: u64, shadow: u64) -> u64 {
    let host = h.call("glMapBufferRange", &[u64::from(target), offset, length, access]) as usize;
    if host == 0 || shadow == 0 {
        if host != 0 {
            h.call("glUnmapBuffer", &[u64::from(target)]);
        }
        return 0;
    }
    let length = length as usize;
    if access & GL_MAP_READ_BIT != 0 {
        // SAFETY: the host's mapping, `length` bytes long while it is mapped.
        let bytes = unsafe { std::slice::from_raw_parts(host as *const u8, length) };
        if p.mem.write(shadow, bytes).is_err() {
            h.call("glUnmapBuffer", &[u64::from(target)]);
            return 0;
        }
    }
    CURRENT.with(|c| c.borrow_mut().maps.insert(target, Mapping { host, shadow, length, access }));
    1
}

/// The shadow's bytes `[from, from + len)` into the host's mapping.
fn copy_out(p: &Process, m: &Mapping, from: usize, len: usize) {
    let Some(end) = from.checked_add(len).filter(|&e| e <= m.length) else { return };
    if let Ok(bytes) = p.mem.read(m.shadow + from as u64, end - from) {
        // SAFETY: the host's mapping, `m.length` bytes long while it is mapped; `end <= m.length`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (m.host + from) as *mut u8, bytes.len()) };
    }
}

fn unmap(h: &Host, p: &Process, target: u32) -> u64 {
    let m = CURRENT.with(|c| c.borrow_mut().maps.remove(&target));
    if let Some(m) = &m {
        if m.access & GL_MAP_WRITE_BIT != 0 && m.access & GL_MAP_FLUSH_EXPLICIT_BIT == 0 {
            copy_out(p, m, 0, m.length);
        }
    }
    let ok = h.call("glUnmapBuffer", &[u64::from(target)]) & 0xff != 0;
    m.map_or(0, |m| m.shadow) | (u64::from(ok) << 63)
}

fn flush_mapped(h: &Host, p: &Process, target: u32, offset: u64, length: u64) {
    CURRENT.with(|c| {
        if let Some(m) = c.borrow().maps.get(&target) {
            copy_out(p, m, offset as usize, length as usize);
        }
    });
    h.call("glFlushMappedBufferRange", &[u64::from(target), offset, length]);
}

// --- images ---------------------------------------------------------------------------------

/// Unpack state saved around an upload of the host's own.
fn with_unpack(h: &Host, es3: bool, f: impl FnOnce()) {
    let alignment = h.get_integer(GL_UNPACK_ALIGNMENT);
    let saved: Option<[i32; 4]> = es3.then(|| {
        [h.get_integer(GL_PIXEL_UNPACK_BUFFER_BINDING), h.get_integer(GL_UNPACK_ROW_LENGTH), h.get_integer(GL_UNPACK_SKIP_ROWS), h.get_integer(GL_UNPACK_SKIP_PIXELS)]
    });
    if es3 {
        h.call("glBindBuffer", &[u64::from(GL_PIXEL_UNPACK_BUFFER), 0]);
        for pname in [GL_UNPACK_ROW_LENGTH, GL_UNPACK_SKIP_ROWS, GL_UNPACK_SKIP_PIXELS] {
            h.call("glPixelStorei", &[u64::from(pname), 0]);
        }
    }
    h.call("glPixelStorei", &[u64::from(GL_UNPACK_ALIGNMENT), 1]);
    f();
    h.call("glPixelStorei", &[u64::from(GL_UNPACK_ALIGNMENT), alignment as u64]);
    if let Some([buffer, row, rows, pixels]) = saved {
        h.call("glPixelStorei", &[u64::from(GL_UNPACK_ROW_LENGTH), row as u64]);
        h.call("glPixelStorei", &[u64::from(GL_UNPACK_SKIP_ROWS), rows as u64]);
        h.call("glPixelStorei", &[u64::from(GL_UNPACK_SKIP_PIXELS), pixels as u64]);
        h.call("glBindBuffer", &[u64::from(GL_PIXEL_UNPACK_BUFFER), buffer as u64]);
    }
}

fn region_generation(shm: &crate::shm::Shm) -> u64 {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, super::native::CONTENT_GENERATION_AT);
    u64::from_le_bytes(g)
}

/// The buffer's pixels into the image's texture (bound for the upload, the old binding restored).
fn upload(h: &Host, es3: bool, img: &Image, first: bool) {
    let Some(bpp) = bytes_per_pixel(img.format) else { return };
    let mut bytes = vec![0u8; img.stride as usize * img.height as usize * bpp];
    if img.shm.read_at(&mut bytes, img.pixels_at).is_err() {
        return;
    }
    let Some((pixels, format, kind)) = from_buffer(&bytes, img.width as usize, img.height as usize, img.format, img.stride as usize) else { return };
    let was = h.get_integer(GL_TEXTURE_BINDING_2D);
    h.call("glBindTexture", &[u64::from(GL_TEXTURE_2D), u64::from(img.texture)]);
    with_unpack(h, es3, || {
        let (w, ht) = (u64::from(img.width), u64::from(img.height));
        if first {
            h.call("glTexParameteri", &[u64::from(GL_TEXTURE_2D), u64::from(GL_TEXTURE_MIN_FILTER), GL_LINEAR as u64]);
            h.call("glTexParameteri", &[u64::from(GL_TEXTURE_2D), u64::from(GL_TEXTURE_MAG_FILTER), GL_LINEAR as u64]);
            h.call("glTexImage2D", &[u64::from(GL_TEXTURE_2D), 0, u64::from(format), w, ht, 0, u64::from(format), u64::from(kind), pixels.as_ptr() as u64]);
        } else {
            h.call("glTexSubImage2D", &[u64::from(GL_TEXTURE_2D), 0, 0, 0, w, ht, u64::from(format), u64::from(kind), pixels.as_ptr() as u64]);
        }
    });
    h.call("glBindTexture", &[u64::from(GL_TEXTURE_2D), was as u64]);
}

#[allow(clippy::too_many_arguments)]
fn image_target(h: &Host, p: &Process, kind: u64, target: u32, image: u64, handle: u64, width: u32, height: u32, format: u32) {
    let ctx = current_context();
    let Some(info) = context_info(ctx) else { return };
    if handle == 0 {
        return;
    }
    let Ok((shm, stride, pixels_at)) = super::native::gralloc_buffer(p, handle) else { return };
    let mut info = info.lock();
    let es3 = info.es3.unwrap_or(false);
    let generation = region_generation(&shm);
    if !info.images.contains_key(&image) {
        let mut texture = 0u32;
        h.call("glGenTextures", &[1, &mut texture as *mut u32 as u64]);
        let mut img = Image { texture, host_image: 0, shm, stride, pixels_at, width, height, format, generation, rendered: false };
        upload(h, es3, &img, true);
        let attribs = [EGL_GL_TEXTURE_LEVEL_KHR, 0, EGL_NONE];
        // SAFETY: a texture of the current context, complete (level 0, no mipmap filter).
        img.host_image = unsafe { (h.egl.create_image)(h.display, ctx, EGL_GL_TEXTURE_2D_KHR, texture as usize, attribs.as_ptr()) };
        if img.host_image == 0 {
            eprintln!(
                "[gl] eglCreateImageKHR(EGL_GL_TEXTURE_2D_KHR) for a {width}x{height} buffer of format {format:#x} failed, eglGetError {:#x}",
                unsafe { (h.egl.get_error)() }
            );
        }
        info.images.insert(image, img);
    } else if let Some(img) = info.images.get_mut(&image) {
        // New pixels in the buffer since: the texture takes them (unless they are its own).
        if img.generation != generation {
            upload(h, es3, img, false);
            img.generation = generation;
        }
    }
    let Some(img) = info.images.get(&image) else { return };
    if img.host_image == 0 {
        return;
    }
    let host_image = img.host_image as u64;
    let name = if kind == 0 {
        h.call("glEGLImageTargetTexture2DOES", &[u64::from(target), host_image]);
        let binding = if target == GL_TEXTURE_EXTERNAL_OES { GL_TEXTURE_BINDING_EXTERNAL_OES } else { GL_TEXTURE_BINDING_2D };
        h.get_integer(binding) as u32
    } else {
        h.call("glEGLImageTargetRenderbufferStorageOES", &[u64::from(target), host_image]);
        h.get_integer(GL_RENDERBUFFER_BINDING) as u32
    };
    info.names.insert((kind as u8, name), image);
}

fn image_destroy(h: &Host, image: u64) {
    let ctx = current_context();
    for (c, info) in objects().contexts.lock().iter() {
        let mut info = info.lock();
        if let Some(img) = info.images.remove(&image) {
            if img.host_image != 0 {
                // SAFETY: a host image this file made.
                unsafe { (h.egl.destroy_image)(h.display, img.host_image) };
            }
            if *c == ctx {
                h.call("glDeleteTextures", &[1, &img.texture as *const u32 as u64]);
            }
        }
        info.names.retain(|_, v| *v != image);
    }
}

/// Every image the current context has rendered into, read back into its buffer.
fn flush_images(h: &Host) {
    let ctx = current_context();
    let Some(info) = context_info(ctx) else { return };
    let mut info = info.lock();
    if !info.images.values().any(|i| i.rendered) {
        return;
    }
    let es3 = info.es3.unwrap_or(false);
    if info.readback_fbo == 0 {
        let mut fbo = 0u32;
        h.call("glGenFramebuffers", &[1, &mut fbo as *mut u32 as u64]);
        info.readback_fbo = fbo;
    }
    let fbo = info.readback_fbo;
    let binding = if es3 { GL_READ_FRAMEBUFFER_BINDING } else { GL_FRAMEBUFFER_BINDING };
    let target = if es3 { GL_READ_FRAMEBUFFER } else { GL_FRAMEBUFFER };
    let was = h.get_integer(binding);
    h.call("glBindFramebuffer", &[u64::from(target), u64::from(fbo)]);
    let pack = save_pack(h, es3);
    for img in info.images.values_mut().filter(|i| i.rendered) {
        h.call("glFramebufferTexture2D", &[u64::from(target), u64::from(GL_COLOR_ATTACHMENT0), u64::from(GL_TEXTURE_2D), u64::from(img.texture), 0]);
        let rgba = read_pixels(h, 0, 0, img.width, img.height);
        if let Some(bytes) = to_buffer(&rgba, img.width as usize, img.height as usize, false, img.format, img.stride as usize) {
            let _ = img.shm.write_at(&bytes, img.pixels_at);
            super::native::bump_generation(&img.shm);
            img.generation = region_generation(&img.shm);
        }
    }
    h.call("glFramebufferTexture2D", &[u64::from(target), u64::from(GL_COLOR_ATTACHMENT0), u64::from(GL_TEXTURE_2D), 0, 0]);
    restore_pack(h, es3, &pack);
    h.call("glBindFramebuffer", &[u64::from(target), was as u64]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_table_is_the_generated_drivers() {
        // The same numbers `tools/gen_gl_forward.py` printed when it generated `device/src/gl/`.
        let src = include_str!("../../device/src/gl/generated.c");
        let line = src.lines().find(|l| l.starts_with("const uint64_t omni_gl_fingerprint")).expect("the fingerprint line");
        assert!(line.contains(&format!("0x{:016x}ull", fingerprint())), "{line} vs {:#018x}", fingerprint());
        let count = src.lines().find(|l| l.starts_with("const uint32_t omni_gl_command_count")).expect("the count line");
        assert!(count.contains(&format!("= {};", commands().len())), "{count}");
    }

    #[test]
    fn ids_are_positions_in_name_order() {
        let names: Vec<&str> = commands().iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(command_id(names[0]), Some(0));
        assert!(command_id("glDrawArrays").is_some());
        assert_eq!(command_id("eglInitialize"), None);
    }

    #[test]
    fn es_versions_parse_from_gl_version() {
        assert_eq!(parse_es_version("OpenGL ES 3.1 Mesa 26.0.8"), Some((3, 1)));
        assert_eq!(parse_es_version("OpenGL ES 3.2 NVIDIA 550.1"), Some((3, 2)));
        assert_eq!(parse_es_version("OpenGL ES 2.0 (ANGLE 2.1.0)"), Some((2, 0)));
        assert_eq!(parse_es_version("4.6 (Core Profile) Mesa"), None);
    }

    #[test]
    fn a_frame_lands_top_down_in_the_buffers_format() {
        // Two rows, bottom first as glReadPixels gives them: red at the bottom, blue at the top.
        let rgba = [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let out = to_buffer(&rgba, 2, 2, true, HAL_RGBA_8888, 3).unwrap();
        assert_eq!(out.len(), 2 * 3 * 4);
        assert_eq!(&out[0..8], &[0, 0, 255, 255, 0, 0, 255, 255], "the top row is the frame's top");
        assert_eq!(&out[12..20], &[255, 0, 0, 255, 255, 0, 0, 255]);
        let bgra = to_buffer(&rgba, 2, 2, true, HAL_BGRA_8888, 2).unwrap();
        assert_eq!(&bgra[0..4], &[255, 0, 0, 255], "blue first in BGRA");
        let rgb565 = to_buffer(&rgba, 2, 2, false, HAL_RGB_565, 2).unwrap();
        assert_eq!(u16::from_le_bytes([rgb565[0], rgb565[1]]), 0xf800);
    }

    #[test]
    fn a_buffer_uploads_tightly_packed_rgba() {
        // A 1x2 BGRX-less RGBX buffer with a stride of 2: alpha forced opaque, padding dropped.
        let bytes = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9];
        let (px, f, t) = from_buffer(&bytes, 1, 2, HAL_RGBX_8888, 2).unwrap();
        assert_eq!(px, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!((f, t), (GL_RGBA, GL_UNSIGNED_BYTE));
    }

    #[test]
    fn the_withheld_extensions_are_what_forwarding_cannot_give() {
        assert!(WITHHELD.contains(&"GL_EXT_buffer_storage"));
        assert!(!WITHHELD.contains(&"GL_OES_EGL_image"));
    }
}
