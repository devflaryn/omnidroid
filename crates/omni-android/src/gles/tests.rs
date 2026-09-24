//! The signature table and the typed callers, tested without a guest or a driver.
//!
//! The callers are tested against **Rust `extern "C"` functions of the same prototypes**, so what is
//! asserted is that the host compiler received each argument where its own convention put it --
//! whichever convention the test is compiled for. That is the property the Windows door depends
//! on: nothing below mentions a register.

use super::egl::android_window_format;
use super::gl::binding_of;
use super::*;

/// The 91 `egl*`/`gl*` symbols `libroblox.so` imports, **as its dynamic symbol table lists them**
/// (MEASURED: the undefined `STT_FUNC` symbols of `lib/arm64-v8a/libroblox.so` in
/// `Roblox-2.738.1397.apk` whose names begin `egl` or `gl`; `tests/gles_loader.rs` re-derives the
/// list from the ELF itself and asserts it equals this one).
pub(crate) const IMPORTED: [&str; 91] = [
    "eglChooseConfig", "eglCreateContext", "eglCreatePbufferSurface", "eglCreateWindowSurface",
    "eglDestroyContext", "eglDestroySurface", "eglGetConfigAttrib", "eglGetCurrentContext",
    "eglGetDisplay", "eglGetError", "eglGetProcAddress", "eglInitialize", "eglMakeCurrent",
    "eglQuerySurface", "eglSwapBuffers", "eglSwapInterval", "eglTerminate", "glActiveTexture",
    "glAttachShader", "glBindAttribLocation", "glBindBuffer", "glBindFramebuffer",
    "glBindRenderbuffer", "glBindTexture", "glBlendFunc", "glBlendFuncSeparate", "glBufferData",
    "glBufferSubData", "glCheckFramebufferStatus", "glClear", "glClearColor", "glClearDepthf",
    "glClearStencil", "glColorMask", "glCompileShader", "glCompressedTexImage2D",
    "glCompressedTexSubImage2D", "glCopyTexSubImage2D", "glCreateProgram", "glCreateShader",
    "glCullFace", "glDeleteBuffers", "glDeleteFramebuffers", "glDeleteProgram",
    "glDeleteRenderbuffers", "glDeleteShader", "glDeleteTextures", "glDepthFunc", "glDepthMask",
    "glDisable", "glDisableVertexAttribArray", "glDrawArrays", "glDrawElements", "glEnable",
    "glEnableVertexAttribArray", "glFramebufferRenderbuffer", "glFramebufferTexture2D",
    "glGenBuffers", "glGenerateMipmap", "glGenFramebuffers", "glGenRenderbuffers", "glGenTextures",
    "glGetActiveUniform", "glGetError", "glGetIntegerv", "glGetProgramInfoLog", "glGetProgramiv",
    "glGetShaderInfoLog", "glGetShaderiv", "glGetString", "glGetUniformLocation", "glLinkProgram",
    "glPixelStorei", "glPolygonOffset", "glReadPixels", "glReleaseShaderCompiler",
    "glRenderbufferStorage", "glScissor", "glShaderSource", "glStencilFunc", "glStencilMask",
    "glStencilOp", "glTexImage2D", "glTexParameterf", "glTexParameterfv", "glTexParameteri",
    "glTexSubImage2D", "glUniform1i", "glUseProgram", "glVertexAttribPointer", "glViewport",
];

#[test]
fn every_imported_symbol_has_a_signature_and_is_bound_by_name() {
    let core: std::collections::BTreeSet<&str> = core_signatures().map(|s| s.name).collect();
    let missing: Vec<&str> = IMPORTED.iter().copied().filter(|n| signature(n).is_none()).collect();
    assert!(missing.is_empty(), "no registry signature for {missing:?}");
    let unbound: Vec<&str> = IMPORTED.iter().copied().filter(|n| !core.contains(n)).collect();
    assert!(unbound.is_empty(), "imported but not core, so not bound by name: {unbound:?}");
}

#[test]
fn the_shapes_are_the_registrys() {
    let s = |n| *signature(n).unwrap_or_else(|| panic!("{n}"));
    // Guest (AAPCS64) classes.
    assert_eq!(s("glUniform4f").params, "IFFFF");
    assert_eq!(s("glClearColor").params, "FFFF");
    assert_eq!(s("glTexSubImage3D").params, "IIIIIIIIIII");
    assert_eq!(s("glVertexAttribPointer").params, "IIIIII");
    assert_eq!(s("glDepthRangef").params, "FF");
    assert_eq!(s("glTexParameterf").params, "IIF");
    assert_eq!(s("eglGetProcAddress").params, "I");
    // Host widths.
    assert_eq!(s("glUniform4f").abi, "WFFFF");
    assert_eq!(s("glColorMask").abi, "BBBB");
    assert_eq!(s("glVertexAttribPointer").abi, "WWWBWP");
    assert_eq!(s("glTexSubImage3D").abi, "WWWWWWWWWWP");
    assert_eq!(s("glMapBufferRange").abi, "WPPW");
    assert_eq!(s("eglCreateWindowSurface").abi, "PPPP");
    // Returns: sign matters for GLint, width for GLboolean, a pointer for glGetString.
    assert_eq!((s("glGetUniformLocation").ret, s("glGetUniformLocation").abi_ret), (b'I', b'S'));
    assert_eq!((s("glIsEnabled").ret, s("glIsEnabled").abi_ret), (b'I', b'B'));
    assert_eq!((s("glGetString").ret, s("glGetString").abi_ret), (b'I', b'P'));
    assert_eq!((s("glGetError").ret, s("glGetError").abi_ret), (b'I', b'W'));
    assert_eq!((s("eglGetError").ret, s("eglGetError").abi_ret), (b'I', b'S'));
    assert_eq!((s("glClear").ret, s("glClear").abi_ret), (b'V', b'V'));
    assert_eq!(s("glGetString").origin, "GL_ES_VERSION_2_0");
    assert_eq!(s("glTexSubImage3D").origin, "GL_ES_VERSION_3_0");
    assert_eq!(s("eglGetPlatformDisplay").origin, "EGL_VERSION_1_5");
    assert_eq!(s("eglSwapBuffersWithDamageKHR").origin, "EGL_KHR_swap_buffers_with_damage");
}

#[test]
fn every_signature_points_at_a_caller_of_its_own_shape() {
    for sig in SIGNATURES {
        let shape = &SHAPES[sig.shape as usize];
        assert_eq!((shape.abi, shape.abi_ret), (sig.abi, sig.abi_ret), "{}", sig.name);
        assert_eq!(sig.params.len(), sig.abi.len(), "{}", sig.name);
        assert!(sig.params.len() < MAX_ARGS, "{}", sig.name);
        for (guest, host) in sig.params.bytes().zip(sig.abi.bytes()) {
            let ok = matches!((guest, host), (b'F', b'F') | (b'D', b'D') | (b'I', b'P' | b'W' | b'B'));
            assert!(ok, "{}: guest class {} with host width {}", sig.name, guest as char, host as char);
        }
        let ret_ok = matches!(
            (sig.ret, sig.abi_ret),
            (b'V', b'V') | (b'F', b'F') | (b'I', b'P' | b'W' | b'S' | b'B')
        );
        assert!(ret_ok, "{}", sig.name);
    }
    // No two shapes are the same shape.
    let distinct: std::collections::BTreeSet<(u8, &str)> =
        SHAPES.iter().map(|s| (s.abi_ret, s.abi)).collect();
    assert_eq!(distinct.len(), SHAPES.len());
}

// ------------------------------------------------------------------ callers, against Rust fns

thread_local! {
    // What the callee saw. Per thread: the callee runs on the test's own thread, and tests run in
    // parallel.
    static SEEN: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn seen() -> Vec<String> {
    SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

fn record(line: String) {
    SEEN.with(|s| s.borrow_mut().push(line));
}

extern "C" fn uniform4f(location: u32, x: f32, y: f32, z: f32, w: f32) {
    record(format!("{location} {x} {y} {z} {w}"));
}

#[allow(clippy::too_many_arguments)]
extern "C" fn tex_sub_image_3d(
    target: u32, level: u32, x: u32, y: u32, z: u32, w: u32, h: u32, d: u32, format: u32,
    kind: u32, pixels: u64,
) {
    record(format!("{target:#x} {level} {x} {y} {z} {w} {h} {d} {format:#x} {kind:#x} {pixels:#x}"));
}

#[allow(clippy::too_many_arguments)]
extern "C" fn nine_floats(p: u64, n: u32, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32, g: f32, h: f32, i: f32) {
    record(format!("{p:#x} {n} {a} {b} {c} {d} {e} {f} {g} {h} {i}"));
}

#[allow(clippy::too_many_arguments)]
extern "C" fn interleaved(a: u32, b: u32, c: u32, d: u64, e: u32, f: f32, g: f32, h: u32, i: u64) {
    record(format!("{a} {b} {c} {d:#x} {e} {f} {g} {h} {i:#x}"));
}

extern "C" fn color_mask(r: u8, g: u8, b: u8, a: u8) {
    record(format!("{r} {g} {b} {a}"));
}

extern "C" fn returns_minus_one(_: u32, _: u64) -> i32 {
    -1
}

extern "C" fn returns_true(_: u32) -> u8 {
    1
}

extern "C" fn returns_high_u32() -> u32 {
    0xFFFF_FFF0
}

extern "C" fn returns_pointer(name: u32) -> u64 {
    0x1234_5678_9ABC_0000 | u64::from(name)
}

extern "C" fn doubles(a: u32, b: f64, c: f64, d: f64) {
    record(format!("{a} {b} {c} {d}"));
}

fn caller(abi_ret: u8, abi: &str) -> &'static Shape {
    SHAPES
        .iter()
        .find(|s| s.abi_ret == abi_ret && s.abi == abi)
        .unwrap_or_else(|| panic!("no shape {}({abi})", abi_ret as char))
}

fn bits(f: f32) -> u64 {
    u64::from(f.to_bits())
}

#[test]
fn a_float_shape_delivers_every_float_where_the_host_convention_puts_it() {
    let _ = seen();
    let shape = caller(b'V', "WFFFF");
    // SAFETY: `uniform4f` has exactly the `V(WFFFF)` prototype.
    unsafe { (shape.call)(uniform4f as *const () as usize, &[7, bits(0.25), bits(-1.5), bits(3.0), bits(1e-3)]) };
    assert_eq!(seen(), vec!["7 0.25 -1.5 3 0.001".to_string()]);
}

#[test]
fn an_eleven_argument_shape_delivers_its_stack_arguments_in_order() {
    let _ = seen();
    let shape = caller(b'V', "WWWWWWWWWWP");
    let lanes = [0x806F, 2, 3, 4, 5, 6, 7, 8, 0x1908, 0x1401, 0xDEAD_BEEF_0000_1111];
    // SAFETY: `tex_sub_image_3d` has exactly the `V(WWWWWWWWWWP)` prototype.
    unsafe { (shape.call)(tex_sub_image_3d as *const () as usize, &lanes) };
    assert_eq!(seen(), vec!["0x806f 2 3 4 5 6 7 8 0x1908 0x1401 0xdeadbeef00001111".to_string()]);
}

#[test]
fn a_ninth_float_goes_past_every_float_register_and_still_arrives() {
    // `V(PWFFFFFFFFF)`: nine floats. SysV has eight vector argument registers, so the ninth is on
    // the stack; Windows x64 has four argument positions in all, so from the fifth argument on
    // everything is. Either way the callee must see these values in this order.
    let _ = seen();
    let mut lanes = vec![0xABCD_0000_1234, 5];
    lanes.extend((1..=9).map(|v| bits(v as f32 * 0.5)));
    // SAFETY: `nine_floats` has exactly the `V(PWFFFFFFFFF)` prototype.
    unsafe { (caller(b'V', "PWFFFFFFFFF").call)(nine_floats as *const () as usize, &lanes) };
    assert_eq!(seen(), vec!["0xabcd00001234 5 0.5 1 1.5 2 2.5 3 3.5 4 4.5".to_string()]);
}

#[test]
fn integers_and_floats_interleaved_keep_their_order() {
    // `V(WWWPWFFWP)`: on SysV the two floats are in xmm0/xmm1 whatever their position, on Windows
    // x64 they are the sixth and seventh arguments and so on the stack. A caller that ordered
    // arguments by class rather than by position would swap them with the integers around them.
    let _ = seen();
    let lanes = [1, 2, 3, 0xFFFF_0000_0000_0004, 5, bits(6.5), bits(-7.25), 8, 0x9999_0000_0000_0009];
    // SAFETY: `interleaved` has exactly the `V(WWWPWFFWP)` prototype.
    unsafe { (caller(b'V', "WWWPWFFWP").call)(interleaved as *const () as usize, &lanes) };
    assert_eq!(seen(), vec!["1 2 3 0xffff000000000004 5 6.5 -7.25 8 0x9999000000000009".to_string()]);
}

#[test]
fn a_glboolean_is_passed_as_the_byte_it_is() {
    let _ = seen();
    // Garbage in the high bits of each lane, as a guest register may carry: the callee sees bytes.
    let lanes = [0xFFFF_FF01, 0xAB00, 0x1_0000_0001, 0];
    // SAFETY: `color_mask` has exactly the `V(BBBB)` prototype.
    unsafe { (caller(b'V', "BBBB").call)(color_mask as *const () as usize, &lanes) };
    assert_eq!(seen(), vec!["1 0 1 0".to_string()]);
}

#[test]
fn returns_are_widened_by_their_own_type() {
    // SAFETY: each function has exactly the prototype of the shape it is called through.
    unsafe {
        assert_eq!((caller(b'S', "WP").call)(returns_minus_one as *const () as usize, &[0, 0]), u64::MAX);
        assert_eq!((caller(b'B', "W").call)(returns_true as *const () as usize, &[0]), 1);
        assert_eq!((caller(b'W', "").call)(returns_high_u32 as *const () as usize, &[]), 0xFFFF_FFF0);
        assert_eq!(
            (caller(b'P', "W").call)(returns_pointer as *const () as usize, &[0x1F01]),
            0x1234_5678_9ABC_1F01
        );
    }
}

#[test]
fn a_double_shape_delivers_doubles() {
    let _ = seen();
    let lanes = [9, 0.5f64.to_bits(), (-2.25f64).to_bits(), 1e10f64.to_bits()];
    // SAFETY: `doubles` has exactly the `V(WDDD)` prototype.
    unsafe { (caller(b'V', "WDDD").call)(doubles as *const () as usize, &lanes) };
    assert_eq!(seen(), vec!["9 0.5 -2.25 10000000000".to_string()]);
}

// ------------------------------------------------------------------ the translations

#[test]
fn native_visual_ids_are_androids_window_formats() {
    assert_eq!(android_window_format(8, 8, 8, 8, false), 1);
    assert_eq!(android_window_format(8, 8, 8, 0, false), 2);
    assert_eq!(android_window_format(5, 6, 5, 0, false), 4);
    assert_eq!(android_window_format(10, 10, 10, 2, false), 0x2b);
    assert_eq!(android_window_format(16, 16, 16, 16, true), 0x16);
    assert_eq!(android_window_format(16, 16, 16, 16, false), 0);
    assert_eq!(android_window_format(4, 4, 4, 4, false), 0);
}

#[test]
fn every_es32_buffer_target_has_its_binding_query() {
    // ES 3.2 table 6.1's thirteen targets and the binding query each names (section 6.1.1).
    let targets = [
        (0x8892, 0x8894), (0x8893, 0x8895), (0x88EB, 0x88ED), (0x88EC, 0x88EF), (0x8A11, 0x8A28),
        (0x8C8E, 0x8C8F), (0x8F36, 0x8F36), (0x8F37, 0x8F37), (0x90D2, 0x90D3), (0x8F3F, 0x8F43),
        (0x90EE, 0x90EF), (0x92C0, 0x92C1), (0x8C2A, 0x8C2A),
    ];
    for (target, binding) in targets {
        assert_eq!(binding_of(target), Some(binding), "{target:#x}");
    }
    assert_eq!(binding_of(0x0DE1), None);
}

#[test]
fn the_pool_slot_symbol_is_not_a_c_identifier() {
    assert_eq!(proc_slot_symbol(3), "gles::proc[3]");
    assert!(signature(&proc_slot_symbol(0)).is_none());
}
