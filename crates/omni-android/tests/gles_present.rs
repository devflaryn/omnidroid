//! **EGL and OpenGL ES from translated ARM64, on a real window and the host's real EGL, with the
//! pixels read back.**
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:94 cargo test -p omni-android --release --test gles_present -- --ignored --test-threads=1
//! ```
//!
//! Every call here is a guest `BLR` into a thunk this boundary bound, with its arguments where
//! AAPCS64 puts them -- integers in `x0`-`x7` and then the stack, floats in `s0`-`s7` -- so what is
//! tested is the whole crossing: the registry's classes read the guest's registers, the typed caller
//! puts them where the host's convention wants them, the host EGL/GLES does the work, and the
//! answer comes back to the guest.
//!
//! Each test prints the host's `GL_RENDERER` and `GL_VERSION`, so a run says which device drew its
//! pixels: under Xvfb Mesa has no DRI device and draws with llvmpipe (on the CPU); on a desktop
//! display it is whatever the host's EGL gives that display.
//!
//! The tests are `#[ignore]`d and gated on `OMNI_GFX_WINDOW_TESTS`: under `--ignored` without the
//! gate each **fails** naming the variable (`VERIFICATION.md` entry 4). The screen capture uses
//! ImageMagick's `import` on the X window, so it runs where the window is an X11 one.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::cell::Cell;
use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::gles::{GlesHost, Gles};
use omni_android::jni::Jni;
use omni_android::ndk::{HostWindowSource, Ndk, WindowSource, SURFACE_CLASS};
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestAddr};
use omni_platform::window::RawWindow;

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

// ------------------------------------------------------------------ the EGL/GL numbers written
const EGL_NONE: u64 = 0x3038;
const EGL_RED_SIZE: u64 = 0x3024;
const EGL_GREEN_SIZE: u64 = 0x3023;
const EGL_BLUE_SIZE: u64 = 0x3022;
const EGL_ALPHA_SIZE: u64 = 0x3021;
const EGL_SURFACE_TYPE: u64 = 0x3033;
const EGL_WINDOW_BIT: u64 = 0x4;
const EGL_RENDERABLE_TYPE: u64 = 0x3040;
const EGL_OPENGL_ES3_BIT: u64 = 0x40;
const EGL_NATIVE_VISUAL_ID: u64 = 0x302E;
const EGL_RECORDABLE_ANDROID: u64 = 0x3142;
const EGL_CONTEXT_CLIENT_VERSION: u64 = 0x3098;
const EGL_VERSION: u64 = 0x3054;
const GL_VENDOR: u64 = 0x1F00;
const GL_RENDERER: u64 = 0x1F01;
const GL_VERSION: u64 = 0x1F02;
const GL_COLOR_BUFFER_BIT: u64 = 0x4000;
const GL_COLOR_CLEAR_VALUE: u64 = 0x0C22;
const GL_RGBA: u64 = 0x1908;
const GL_RGBA8: u64 = 0x8058;
const GL_UNSIGNED_BYTE: u64 = 0x1401;
const GL_FLOAT: u64 = 0x1406;
const GL_TEXTURE_2D: u64 = 0x0DE1;
const GL_TEXTURE_3D: u64 = 0x806F;
const GL_TEXTURE_MIN_FILTER: u64 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u64 = 0x2800;
const GL_NEAREST: u64 = 0x2600;
const GL_FRAMEBUFFER: u64 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u64 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u64 = 0x8CD5;
const GL_VERTEX_SHADER: u64 = 0x8B31;
const GL_FRAGMENT_SHADER: u64 = 0x8B30;
const GL_COMPILE_STATUS: u64 = 0x8B81;
const GL_LINK_STATUS: u64 = 0x8B82;
const GL_TRIANGLE_STRIP: u64 = 0x0005;
const GL_ARRAY_BUFFER: u64 = 0x8892;
const GL_STATIC_DRAW: u64 = 0x88E4;
const GL_MAP_READ_BIT: u64 = 0x1;
const GL_MAP_WRITE_BIT: u64 = 0x2;
const GL_MAP_INVALIDATE_RANGE_BIT: u64 = 0x4;
const GL_MAP_FLUSH_EXPLICIT_BIT: u64 = 0x10;
const GL_BUFFER_MAP_POINTER: u64 = 0x88BD;
const GL_NO_ERROR: u64 = 0;

/// Clear colours exact in eight bits (`round(v * 255)` is whole for each), all channels distinct.
const COLOUR_A: [f32; 4] = [0.2, 0.6, 0.8, 1.0];
const BYTES_A: [u8; 4] = [51, 153, 204, 255];
const COLOUR_B: [f32; 4] = [0.8, 0.4, 0.0, 1.0];
const BYTES_B: [u8; 4] = [204, 102, 0, 255];

fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "this test was run with --ignored but {GATE} is not set to 1. It opens a window, loads the \
         host's EGL and GLES, draws into the window from guest code and reads the pixels back; it \
         will not pretend to have passed on a machine that cannot do that. Set {GATE}=1 (and \
         DISPLAY) to run it, or drop --ignored to skip it visibly."
    );
}

// ======================================================================= the fixture

struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    ndk: Arc<Ndk>,
    gles: Arc<Gles>,
    host: Arc<omni_gfx::GfxGlesHost>,
    boundary: Arc<Boundary>,
    next: Cell<usize>,
    window: omni_platform::window::Window,
    raw: RawWindow,
}

const ARENA_AT: usize = 0x800;
const SLOTS: usize = 4096;

impl Fixture {
    fn new(title: &str) -> Self {
        let mut window = omni_platform::window::Window::new(
            &omni_platform::window::WindowDesc::new(title, 320, 240),
        )
        .unwrap_or_else(|err| panic!("could not create the window: {err}"));
        window.show();
        let _ = window.poll_events().count();
        let raw = window.raw();

        let guest = Guest::new();
        let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
        let ndk = Ndk::new(Arc::clone(&guest.space)).expect("an NDK instance");
        let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
        let gles = Gles::new(Arc::clone(&guest.space));
        let builder = guest.boundary(SLOTS);
        bionic.bind_into(&builder).expect("bind bionic");
        ndk.bind_into(&builder).expect("bind the NDK");
        jni.install_into(&builder).expect("install JNI");
        let bound = gles.bind_into(&builder).expect("bind EGL and GLES");
        assert_eq!(bound, omni_android::gles::bound_symbol_count());
        bionic.set_log_to_stderr(false);
        let host = omni_gfx::GfxGlesHost::new();
        gles.set_host(Arc::clone(&host) as Arc<dyn GlesHost>);
        let source = HostWindowSource::watching(&window).expect("a source watching the window");
        ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);
        let boundary = builder.finish();
        Self {
            guest,
            bionic,
            jni,
            ndk,
            gles,
            host,
            boundary,
            next: Cell::new(ARENA_AT),
            window,
            raw,
        }
    }

    fn thunk(&self, symbol: &str) -> u64 {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
            as u64
    }

    fn alloc(&self, len: usize) -> u64 {
        let offset = (self.next.get() + 15) & !15;
        assert!(offset + len < harness::DATA_BYTES, "the guest arena is full");
        self.next.set(offset + len);
        (self.guest.data + offset) as u64
    }

    fn bytes(&self, image: &[u8]) -> u64 {
        let at = self.alloc(image.len().max(8));
        self.guest.write_bytes(at as GuestAddr, image);
        at
    }

    fn cstr(&self, text: &str) -> u64 {
        self.bytes(&text.bytes().chain(std::iter::once(0)).collect::<Vec<_>>())
    }

    fn i32s(&self, values: &[u64]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| (*v as u32).to_le_bytes()).collect();
        self.bytes(&bytes)
    }

    fn read(&self, at: u64, len: usize) -> Vec<u8> {
        let ptr = self.guest.space.ptr(at as GuestAddr, len).expect("inside the space");
        // SAFETY: the harness's data region is eagerly committed and nothing else is running.
        unsafe { std::slice::from_raw_parts(ptr, len).to_vec() }
    }

    fn read_i32(&self, at: u64) -> i32 {
        i32::from_le_bytes(self.read(at, 4).try_into().expect("four"))
    }

    fn read_f32(&self, at: u64) -> f32 {
        f32::from_le_bytes(self.read(at, 4).try_into().expect("four"))
    }

    fn run(&self, entry: GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish bionic");
        let _jni = self.jni.activate().expect("publish JNI");
        let _ndk = self.ndk.activate();
        let _gles = self.gles.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    /// Call `target` from guest code: `ints` in `x0`-`x7` then on the stack, `floats` in
    /// `s0`-`s7`. Returns `x0`.
    fn call(&self, target: u64, ints: &[u64], floats: &[f32]) -> Result<u64, AbiError> {
        assert!(floats.len() <= 8 && ints.len() <= 12);
        let stacked: Vec<u64> = ints.iter().skip(8).copied().collect();
        let frame = ((stacked.len() * 8) + 15) & !15;
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(9, target);
        if frame > 0 {
            asm.push(sub_imm(31, 31, frame as u32));
            for (index, value) in stacked.iter().enumerate() {
                asm.mov(10, *value);
                asm.push(str_imm(10, 31, (index * 8) as u32));
            }
        }
        for (index, value) in floats.iter().enumerate() {
            asm.mov(10, u64::from(value.to_bits()));
            // FMOV Sd, Wn -- `0 0 0 11110 00 1 00 111 000000 Rn Rd`.
            asm.push(0x1E27_0000 | (10 << 5) | index as u32);
        }
        for (index, value) in ints.iter().take(8).enumerate() {
            asm.mov(index as u32, *value);
        }
        asm.push(blr(9));
        if frame > 0 {
            asm.push(add_imm(31, 31, frame as u32));
        }
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        let exit = self.run(entry)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    fn gl(&self, name: &str, ints: &[u64]) -> u64 {
        self.call(self.thunk(name), ints, &[]).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn glf(&self, name: &str, ints: &[u64], floats: &[f32]) -> u64 {
        self.call(self.thunk(name), ints, floats).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn no_gl_error(&self, after: &str) {
        let error = self.gl("glGetError", &[]) as u32;
        assert_eq!(u64::from(error), GL_NO_ERROR, "glGetError after {after}: {error:#x}");
    }

    /// The guest's `ANativeWindow *` for this fixture's window.
    fn native_window(&self) -> u64 {
        let surface = self.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
        let window = self
            .call(self.thunk("ANativeWindow_fromSurface"), &[0, surface], &[])
            .expect("fromSurface");
        assert_ne!(window, 0);
        window
    }

    /// Display, config, window surface and an ES 3 context, current on this thread.
    fn bring_up(&self) -> (u64, u64, u64, u64) {
        let display = self.gl("eglGetDisplay", &[0]);
        assert_ne!(display, 0, "EGL_DEFAULT_DISPLAY must resolve to the host's display");
        let versions = self.alloc(8);
        assert_eq!(self.gl("eglInitialize", &[display, versions, versions + 4]) as u32, 1);
        let (major, minor) = (self.read_i32(versions), self.read_i32(versions + 4));
        assert!((major, minor) >= (1, 4), "EGL {major}.{minor}");
        let attributes = self.i32s(&[
            EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
            EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE,
        ]);
        let config_at = self.alloc(8);
        let count_at = self.alloc(4);
        assert_eq!(
            self.gl("eglChooseConfig", &[display, attributes, config_at, 1, count_at]) as u32,
            1
        );
        assert!(self.read_i32(count_at) >= 1, "no ES 3 window config");
        let config = self.guest.read_u64(config_at as GuestAddr);
        let window = self.native_window();
        let surface = self.gl("eglCreateWindowSurface", &[display, config, window, 0]);
        assert_ne!(surface, 0, "eglCreateWindowSurface: eglGetError {:#x}", self.gl("eglGetError", &[]));
        let context_attributes = self.i32s(&[EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE]);
        let context = self.gl("eglCreateContext", &[display, config, 0, context_attributes]);
        assert_ne!(context, 0, "eglCreateContext: eglGetError {:#x}", self.gl("eglGetError", &[]));
        assert_eq!(self.gl("eglMakeCurrent", &[display, surface, surface, context]) as u32, 1);
        let renderer = self.guest_string(self.gl("glGetString", &[GL_RENDERER]));
        let version = self.guest_string(self.gl("glGetString", &[GL_VERSION]));
        eprintln!("GLES: renderer \"{renderer}\", version \"{version}\" (the host's; this run's pixels are drawn by it)");
        (display, config, surface, context)
    }

    fn tear_down(&self, display: u64, surface: u64, context: u64) {
        assert_eq!(self.gl("eglMakeCurrent", &[display, 0, 0, 0]) as u32, 1);
        assert_eq!(self.gl("eglDestroyContext", &[display, context]) as u32, 1);
        assert_eq!(self.gl("eglDestroySurface", &[display, surface]) as u32, 1);
        assert_eq!(self.gl("eglTerminate", &[display]) as u32, 1);
    }

    /// A NUL-terminated string at a guest address, read **as guest memory**: an address outside the
    /// guest's space fails here.
    fn guest_string(&self, at: u64) -> String {
        assert_ne!(at, 0);
        let bytes = self
            .boundary
            .mem()
            .cstr(at as GuestAddr, omni_android::Blame::new("test", 0, 0))
            .unwrap_or_else(|e| panic!("{at:#x} is not a string in guest memory: {e}"));
        String::from_utf8(bytes).expect("UTF-8")
    }

    /// The host's own answer to `glGetString(name)`, asked of the host library directly.
    fn host_string(&self, name: u64) -> (u64, String) {
        let proc = self.host.proc_address("glGetString").expect("loaded").expect("glGetString");
        let address = proc.address() as u64;
        // SAFETY: the host's `const GLubyte *glGetString(GLenum)`, on the thread whose context is
        // current (this one: the guest runs on the test thread).
        let text = unsafe {
            core::mem::transmute::<u64, extern "C" fn(u32) -> *const core::ffi::c_char>(address)(
                name as u32,
            )
        };
        assert!(!text.is_null());
        // SAFETY: a non-NULL glGetString result is a static NUL-terminated string.
        let s = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned();
        (text as u64, s)
    }

    fn read_pixel(&self, x: u64, y: u64) -> [u8; 4] {
        let out = self.alloc(4);
        self.gl("glReadPixels", &[x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out]);
        self.read(out, 4).try_into().expect("four")
    }

    /// The window's pixel at (x, y) as the X server has it, captured with ImageMagick `import`.
    fn capture(&self, x: u32, y: u32) -> [u8; 3] {
        let RawWindow::Xlib { window, .. } = self.raw else { panic!("an X11 window") };
        let output = std::process::Command::new("import")
            .args(["-window", &format!("{window:#x}"), "-crop", &format!("1x1+{x}+{y}"), "txt:-"])
            .output()
            .expect("ImageMagick `import` must be installed for the capture");
        assert!(output.status.success(), "import: {}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().last().expect("a pixel line");
        let hex = line.split_whitespace().find(|w| w.starts_with('#')).expect("#RRGGBB");
        let v = |i: usize| u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).expect("hex");
        // `import` prints 8-bit channels as #RRGGBB and 16-bit ones as #RRRRGGGGBBBB.
        if hex.len() == 13 {
            let w = |i: usize| (u16::from_str_radix(&hex[1 + 4 * i..5 + 4 * i], 16).expect("hex") >> 8) as u8;
            [w(0), w(1), w(2)]
        } else {
            [v(0), v(1), v(2)]
        }
    }

    fn present(&self, display: u64, surface: u64, colour: [f32; 4]) {
        self.glf("glClearColor", &[], &colour);
        self.gl("glClear", &[GL_COLOR_BUFFER_BIT]);
        assert_eq!(self.gl("eglSwapBuffers", &[display, surface]) as u32, 1);
        // Mesa's X11 swap is an XPutImage/Present on the window's connection: wait until the
        // server has it before the capture asks, by a round trip on that connection.
        let _ = self.window.client_size();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ============================================================================ the tests

/// EGL from the default display to a presented frame, and the frame seen three ways: the guest's
/// `glReadPixels`, the census, and a capture of the X window -- which is first shown seeing a known
/// colour and then shown seeing it change (VERIFICATION entry 19).
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_cleared_frame_is_presented_from_guest_code_and_the_window_shows_it() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: a guest frame");
    let (display, config, surface, context) = f.bring_up();

    // glGetString: the host's own text, at a guest address, never the host's pointer.
    for name in [GL_VENDOR, GL_RENDERER, GL_VERSION] {
        let guest = f.gl("glGetString", &[name]);
        let (host_pointer, host_text) = f.host_string(name);
        assert_ne!(guest, host_pointer, "the guest was handed the host's pointer");
        assert!(f.guest.space.contains(guest as GuestAddr, 1), "{guest:#x} is not guest memory");
        assert_eq!(f.guest_string(guest), host_text);
        assert_eq!(f.gl("glGetString", &[name]), guest, "a static string has one address");
    }
    assert!(f.guest_string(f.gl("glGetString", &[GL_VERSION])).starts_with("OpenGL ES 3."));
    let egl_version = f.guest_string(f.gl("eglQueryString", &[display, EGL_VERSION]));
    assert!(egl_version.starts_with("1."), "{egl_version}");

    // EGL_NATIVE_VISUAL_ID is Android's WINDOW_FORMAT_*, from the config's own channels.
    let value = f.alloc(4);
    assert_eq!(f.gl("eglGetConfigAttrib", &[display, config, EGL_ALPHA_SIZE, value]) as u32, 1);
    let alpha = f.read_i32(value);
    assert_eq!(f.gl("eglGetConfigAttrib", &[display, config, EGL_NATIVE_VISUAL_ID, value]) as u32, 1);
    assert_eq!(f.read_i32(value), if alpha == 8 { 1 } else { 2 }, "alpha {alpha}");

    // Floats arrive: the clear colour is read back exactly as it was passed in s0-s3.
    f.glf("glClearColor", &[], &COLOUR_A);
    let clear = f.alloc(16);
    f.gl("glGetFloatv", &[GL_COLOR_CLEAR_VALUE, clear]);
    let read: Vec<f32> = (0..4).map(|i| f.read_f32(clear + 4 * i)).collect();
    assert_eq!(read, COLOUR_A.to_vec());
    f.glf("glClearColor", &[], &[0.25, -0.5, 1.5, 0.125]);
    f.gl("glGetFloatv", &[GL_COLOR_CLEAR_VALUE, clear]);
    let read: Vec<f32> = (0..4).map(|i| f.read_f32(clear + 4 * i)).collect();
    // ES 3.2 section 15.2.3: glClearColor values are not clamped by the call.
    assert_eq!(read, vec![0.25, -0.5, 1.5, 0.125]);

    let (width, height) = f.window.client_size().expect("a client area");
    f.gl("glViewport", &[0, 0, u64::from(width), u64::from(height)]);

    // Colour A: read back from the back buffer before the swap, then presented and captured.
    f.glf("glClearColor", &[], &COLOUR_A);
    f.gl("glClear", &[GL_COLOR_BUFFER_BIT]);
    assert_eq!(f.read_pixel(10, 10), BYTES_A);
    f.no_gl_error("the clear");
    let before = f.gles.presents();
    assert_eq!(f.gl("eglSwapBuffers", &[display, surface]) as u32, 1);
    assert_eq!(f.gles.presents(), before + 1);
    let _ = f.window.client_size();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let seen_a = f.capture(width / 2, height / 2);
    assert_eq!(seen_a, [BYTES_A[0], BYTES_A[1], BYTES_A[2]], "the capture of the window");

    // Colour B: the capture must follow -- an instrument that returned a stale or default image
    // would still say A.
    f.present(display, surface, COLOUR_B);
    let seen_b = f.capture(width / 2, height / 2);
    assert_eq!(seen_b, [BYTES_B[0], BYTES_B[1], BYTES_B[2]], "the capture after the second frame");
    eprintln!("GLES: capture saw {seen_a:?} then {seen_b:?}; {} presents counted", f.gles.presents());

    let report = f.gles.report();
    eprintln!("{report}");
    assert!(report.contains("eglCreateWindowSurface"), "{report}");
    f.tear_down(display, surface, context);
}

/// `EGL_RECORDABLE_ANDROID` in a config request: a desktop EGL rejects it, so it is dropped and
/// the drop is in the census; the rest of the request is honoured.
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn an_android_only_config_attribute_is_dropped_and_recorded() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: config");
    let display = f.gl("eglGetDisplay", &[0]);
    let versions = f.alloc(8);
    assert_eq!(f.gl("eglInitialize", &[display, versions, versions + 4]) as u32, 1);
    let attributes = f.i32s(&[
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_RECORDABLE_ANDROID, 1, EGL_RED_SIZE, 8, EGL_NONE,
    ]);
    let configs = f.alloc(8 * 4);
    let count = f.alloc(4);
    let ok = f.gl("eglChooseConfig", &[display, attributes, configs, 4, count]) as u32;
    assert_eq!(ok, 1, "eglGetError {:#x}", f.gl("eglGetError", &[]));
    assert!(f.read_i32(count) >= 1);
    // Every config returned has what was asked apart from the dropped attribute.
    let config = f.guest.read_u64(configs as GuestAddr);
    let value = f.alloc(4);
    f.gl("eglGetConfigAttrib", &[display, config, EGL_RED_SIZE, value]);
    assert!(f.read_i32(value) >= 8);
    let substitutions = f.gles.substitutions();
    assert!(
        substitutions.iter().any(|s| s.call == "eglChooseConfig" && s.what.contains("EGL_RECORDABLE_ANDROID")),
        "{substitutions:?}"
    );
    assert_eq!(f.gl("eglTerminate", &[display]) as u32, 1);
}

const VERTEX_SHADER: &str = "#version 300 es
in vec2 position;
out vec2 uv;
void main() { uv = position * 0.5 + 0.5; gl_Position = vec4(position, 0.0, 1.0); }
";

const FRAGMENT_SHADER: &str = "#version 300 es
precision mediump float;
uniform sampler2D image;
uniform vec4 tint;
in vec2 uv;
out vec4 colour;
void main() { colour = texture(image, uv) * tint; }
";

/// A real GLES 3 shader drawing a 2x2 texture across the window through a client-side vertex
/// array, tinted by a `vec4` uniform set with `glUniform4f` -- four floats in `s0`-`s3` after an
/// integer in `x0` -- and the uniform and the pixels read back.
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_textured_shaded_draw_from_guest_code_reads_back_as_drawn() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: a shaded draw");
    let (display, _config, surface, context) = f.bring_up();
    let (width, height) = f.window.client_size().expect("a client area");
    f.gl("glViewport", &[0, 0, u64::from(width), u64::from(height)]);

    let compile = |kind: u64, source: &str| -> u64 {
        let shader = f.gl("glCreateShader", &[kind]);
        let text = f.cstr(source);
        let strings = f.alloc(8);
        f.guest.write_u64(strings as GuestAddr, text);
        f.gl("glShaderSource", &[shader, 1, strings, 0]);
        f.gl("glCompileShader", &[shader]);
        let status = f.alloc(4);
        f.gl("glGetShaderiv", &[shader, GL_COMPILE_STATUS, status]);
        if f.read_i32(status) != 1 {
            let log = f.alloc(1024);
            f.gl("glGetShaderInfoLog", &[shader, 1024, 0, log]);
            panic!("the shader did not compile: {}", f.guest_string(log));
        }
        shader
    };
    let program = f.gl("glCreateProgram", &[]);
    f.gl("glAttachShader", &[program, compile(GL_VERTEX_SHADER, VERTEX_SHADER)]);
    f.gl("glAttachShader", &[program, compile(GL_FRAGMENT_SHADER, FRAGMENT_SHADER)]);
    f.gl("glBindAttribLocation", &[program, 0, f.cstr("position")]);
    f.gl("glLinkProgram", &[program]);
    let status = f.alloc(4);
    f.gl("glGetProgramiv", &[program, GL_LINK_STATUS, status]);
    assert_eq!(f.read_i32(status), 1, "the program did not link");
    f.gl("glUseProgram", &[program]);

    // A 2x2 texture: red, green / blue, white (rows bottom-up in GL).
    let texels = f.bytes(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]);
    let names = f.alloc(4);
    f.gl("glGenTextures", &[1, names]);
    let texture = f.read_i32(names) as u64;
    f.gl("glBindTexture", &[GL_TEXTURE_2D, texture]);
    f.gl("glTexParameteri", &[GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST]);
    f.gl("glTexParameteri", &[GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST]);
    f.gl("glPixelStorei", &[0x0CF5, 1]); // GL_UNPACK_ALIGNMENT
    f.gl("glTexImage2D", &[GL_TEXTURE_2D, 0, GL_RGBA, 2, 2, 0, GL_RGBA, GL_UNSIGNED_BYTE, texels]);
    f.gl("glUniform1i", &[f.gl("glGetUniformLocation", &[program, f.cstr("image")]), 0]);
    let tint = f.gl("glGetUniformLocation", &[program, f.cstr("tint")]) as u32 as i32;
    assert!(tint >= 0, "tint has a location");
    f.glf("glUniform4f", &[tint as u32 as u64], &[1.0, 0.6, 0.2, 1.0]);
    let back = f.alloc(16);
    f.gl("glGetUniformfv", &[program, tint as u32 as u64, back]);
    let read: Vec<f32> = (0..4).map(|i| f.read_f32(back + 4 * i)).collect();
    assert_eq!(read, vec![1.0, 0.6, 0.2, 1.0], "glUniform4f's floats, read back");

    // The quad from a client-side array in guest memory: no buffer is bound, so the driver reads
    // this address at the draw.
    let quad: Vec<u8> =
        [-1.0f32, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0].iter().flat_map(|v| v.to_le_bytes()).collect();
    let vertices = f.bytes(&quad);
    f.gl("glBindBuffer", &[GL_ARRAY_BUFFER, 0]);
    f.gl("glVertexAttribPointer", &[0, 2, GL_FLOAT, 0, 0, vertices]);
    f.gl("glEnableVertexAttribArray", &[0]);
    f.glf("glClearColor", &[], &[0.0, 0.0, 0.0, 1.0]);
    f.gl("glClear", &[GL_COLOR_BUFFER_BIT]);
    f.gl("glDrawArrays", &[GL_TRIANGLE_STRIP, 0, 4]);
    f.no_gl_error("the draw");
    // Quadrant centres: texel * tint, with the tint's 0.6 and 0.2 rounding in eight bits.
    let (qx, qy) = (u64::from(width / 4), u64::from(height / 4));
    let expect = |t: [f32; 3]| {
        [(t[0] * 255.0).round() as u8, (t[1] * 0.6 * 255.0).round() as u8, (t[2] * 0.2 * 255.0).round() as u8, 255]
    };
    let near = |got: [u8; 4], want: [u8; 4]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1);
    let cases = [
        ((qx, qy), expect([1.0, 0.0, 0.0])),
        ((3 * qx, qy), expect([0.0, 1.0, 0.0])),
        ((qx, 3 * qy), expect([0.0, 0.0, 1.0])),
        ((3 * qx, 3 * qy), expect([1.0, 1.0, 1.0])),
    ];
    for ((x, y), want) in cases {
        let got = f.read_pixel(x, y);
        assert!(near(got, want), "pixel ({x}, {y}): {got:?}, expected {want:?}");
    }
    assert_eq!(f.gl("eglSwapBuffers", &[display, surface]) as u32, 1);
    f.tear_down(display, surface, context);
}

/// `glTexSubImage3D` takes eleven arguments: AAPCS64 puts the last three -- the format, the type
/// and the pixel pointer -- on the guest's stack. One texel is written through it and read back.
/// Also: an ES 3 command's `eglGetProcAddress` answer is the same thunk the import binding has.
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn an_eleven_argument_call_takes_its_last_three_from_the_guest_stack() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: glTexSubImage3D");
    let (display, _config, surface, context) = f.bring_up();
    let by_name = f.gl("eglGetProcAddress", &[f.cstr("glTexSubImage3D")]);
    assert_eq!(by_name, f.thunk("glTexSubImage3D"), "one address per command");

    let names = f.alloc(4);
    f.gl("glGenTextures", &[1, names]);
    let texture = f.read_i32(names) as u64;
    f.gl("glBindTexture", &[GL_TEXTURE_3D, texture]);
    f.gl("glTexParameteri", &[GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST]);
    f.gl("glTexParameteri", &[GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST]);
    let zeros = f.bytes(&[0u8; 4 * 4 * 4 * 4]);
    // glTexImage3D: ten arguments, two of them on the stack.
    f.gl("glTexImage3D", &[GL_TEXTURE_3D, 0, GL_RGBA8, 4, 4, 4, 0, GL_RGBA, GL_UNSIGNED_BYTE, zeros]);
    f.no_gl_error("glTexImage3D");
    let texel = f.bytes(&[10, 20, 30, 40]);
    // (x, y, z) = (1, 2, 3), one texel.
    f.gl(
        "glTexSubImage3D",
        &[GL_TEXTURE_3D, 0, 1, 2, 3, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, texel],
    );
    f.no_gl_error("glTexSubImage3D");
    let framebuffers = f.alloc(4);
    f.gl("glGenFramebuffers", &[1, framebuffers]);
    f.gl("glBindFramebuffer", &[GL_FRAMEBUFFER, f.read_i32(framebuffers) as u64]);
    f.gl("glFramebufferTextureLayer", &[GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, texture, 0, 3]);
    assert_eq!(f.gl("glCheckFramebufferStatus", &[GL_FRAMEBUFFER]) as u32 as u64, GL_FRAMEBUFFER_COMPLETE);
    assert_eq!(f.read_pixel(1, 2), [10, 20, 30, 40], "the texel written at (1, 2, 3)");
    assert_eq!(f.read_pixel(2, 1), [0, 0, 0, 0], "and nowhere else");
    f.gl("glBindFramebuffer", &[GL_FRAMEBUFFER, 0]);
    let _ = surface;
    f.tear_down(display, surface, context);
}

/// A buffer mapping: the guest gets guest memory, filled from the buffer when the access says
/// its contents are defined, copied back on unmap -- or, with `GL_MAP_FLUSH_EXPLICIT_BIT`, only the
/// flushed range.
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_buffer_mapping_round_trips_through_guest_memory() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: mappings");
    let (display, _config, surface, context) = f.bring_up();
    let names = f.alloc(4);
    f.gl("glGenBuffers", &[1, names]);
    f.gl("glBindBuffer", &[GL_ARRAY_BUFFER, f.read_i32(names) as u64]);
    let pattern: Vec<u8> = (0..64u8).collect();
    f.gl("glBufferData", &[GL_ARRAY_BUFFER, 64, f.bytes(&pattern), GL_STATIC_DRAW]);

    // Read: the guest sees the buffer's bytes, at a guest address.
    let map = |offset: u64, length: u64, access: u64| -> u64 {
        let at = f.gl("glMapBufferRange", &[GL_ARRAY_BUFFER, offset, length, access]);
        assert_ne!(at, 0, "glMapBufferRange: glGetError {:#x}", f.gl("glGetError", &[]));
        assert!(f.guest.space.contains(at as GuestAddr, length as usize), "{at:#x} is not guest memory");
        at
    };
    let unmap = || assert_eq!(f.gl("glUnmapBuffer", &[GL_ARRAY_BUFFER]) as u8, 1);
    let at = map(0, 64, GL_MAP_READ_BIT);
    assert_eq!(f.read(at, 64), pattern);
    let pointer = f.alloc(8);
    f.gl("glGetBufferPointerv", &[GL_ARRAY_BUFFER, GL_BUFFER_MAP_POINTER, pointer]);
    assert_eq!(f.guest.read_u64(pointer as GuestAddr), at, "GL_BUFFER_MAP_POINTER is the shadow");
    unmap();

    // Write a sub-range, invalidated: the guest's bytes reach the buffer at unmap.
    let at = map(16, 8, GL_MAP_WRITE_BIT | GL_MAP_INVALIDATE_RANGE_BIT);
    f.guest.write_bytes(at as GuestAddr, &[0xAA; 8]);
    unmap();
    // Write without invalidating, touching only two bytes: the other six must survive.
    let at = map(32, 8, GL_MAP_WRITE_BIT);
    f.guest.write_bytes(at as GuestAddr + 3, &[0xBB, 0xBB]);
    unmap();
    // Explicit flush: only the flushed half is the guest's.
    let at = map(48, 16, GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT);
    f.guest.write_bytes(at as GuestAddr, &[0xCC; 16]);
    f.gl("glFlushMappedBufferRange", &[GL_ARRAY_BUFFER, 0, 8]);
    unmap();
    f.no_gl_error("the mappings");

    let at = map(0, 64, GL_MAP_READ_BIT);
    let now = f.read(at, 64);
    unmap();
    let mut want = pattern.clone();
    want[16..24].fill(0xAA);
    want[35..37].fill(0xBB);
    want[48..56].fill(0xCC);
    assert_eq!(now, want, "the buffer after three mapped writes");
    f.tear_down(display, surface, context);
}

/// `eglGetProcAddress` is the host's truth: a thunk for a name the host has, NULL for one it lacks
/// -- and NULL for a name no GLES or EGL registry has, whatever a dispatch layer would answer.
#[test]
#[ignore = "opens a window and the host EGL; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn get_proc_address_answers_null_for_what_the_host_lacks() {
    require_gate();
    let _serial = serialized();
    let f = Fixture::new("Omnidroid - GLES: eglGetProcAddress");
    let (display, _config, surface, context) = f.bring_up();
    let ask = |name: &str| f.gl("eglGetProcAddress", &[f.cstr(name)]);

    // An extension the host has: a thunk of this boundary, never the host's address.
    let has = ask("glDrawElementsBaseVertexOES");
    let host_has = f.host.proc_address("glDrawElementsBaseVertexOES").expect("loaded");
    assert_eq!(has != 0, host_has.is_some());
    if has != 0 {
        assert!(f.boundary.symbol_at(has as GuestAddr).is_some(), "a thunk of this boundary");
        assert_eq!(ask("glDrawElementsBaseVertexOES"), has, "one address per name");
    }
    // An EGL extension function the host lacks (asked of the host directly first).
    let missing = ["eglCreateStreamKHR", "eglQueryDisplayAttribNV", "eglStreamConsumerGLTextureExternalKHR"]
        .into_iter()
        .find(|name| f.host.proc_address(name).expect("loaded").is_none())
        .expect("the host lacks at least one of three NVIDIA/stream EGL entry points");
    assert_eq!(ask(missing), 0, "{missing}: the host has none, so NULL");
    // Not a command of any registry: NULL, though a GL dispatch layer answers every gl* name.
    assert_eq!(ask("glNotACommandOfAnyRegistry"), 0);
    let requests = f.gles.requests();
    assert!(requests.iter().any(|r| r.name == missing));
    f.tear_down(display, surface, context);
}
