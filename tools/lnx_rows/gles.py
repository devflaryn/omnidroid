"""lnx-gles-: the guest's EGL/GLES (omni_android::gles) and the host EGL behind it (omni_gfx::gles).

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

**The LIVE rows need `Xvfb :94` running** (no window manager): they drive translated ARM64 through
the thunks into the host's real EGL (Mesa, llvmpipe under Xvfb) and read the pixels back. The
pre-flight runs each command on the clean tree first, so a missing server fails loudly.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import HOME, with_env  # noqa: E402

MOD_RS = "crates/omni-android/src/gles/mod.rs"
EGL_RS = "crates/omni-android/src/gles/egl.rs"
GL_RS = "crates/omni-android/src/gles/gl.rs"
SIGNATURES_RS = "crates/omni-android/src/gles/signatures.rs"

_CHECKOUT = os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
DYNARMIC_ENV = {"OMNIDROID_DYNARMIC_BUILD_DIR": os.path.join(HOME, "odb", f"dyn-{_CHECKOUT}")}

UNIT = with_env(DYNARMIC_ENV, ["cargo", "test", "-p", "omni-android", "--release", "--no-fail-fast",
                               "--lib", "gles::"])
LIVE = with_env({**DYNARMIC_ENV, "OMNI_GFX_WINDOW_TESTS": "1", "DISPLAY": ":94"},
                ["cargo", "test", "-p", "omni-android", "--release", "--no-fail-fast",
                 "--test", "gles_present", "--", "--ignored", "--test-threads=1"])
GFX_UNIT = ["cargo", "test", "-p", "omni-gfx", "--release", "--no-fail-fast", "--lib", "gles::"]

ROWS = [
    # --- the calling convention -------------------------------------------------------------------
    # A float parameter read out of the next X register instead of the next S register: glClearColor
    # gets four integers' worth of garbage, and the live test reads GL_COLOR_CLEAR_VALUE back.
    ("lnx-gles-A1", "A", "a float argument is read from the integer registers",
     MOD_RS,
     """                b'F' => u64::from(args.next_f32()?.to_bits()),""",
     """                b'F' => args.next_u64()?,""",
     LIVE),
    # Stack arguments never read: only the first eight parameters come from the guest, so
    # glTexSubImage3D's format, type and pixel pointer (AAPCS64 puts them on the stack) are zero.
    ("lnx-gles-A2", "A", "arguments past the eighth are not read from the guest stack",
     MOD_RS,
     """        for (lane, class) in lanes.iter_mut().zip(signature.params.bytes()) {""",
     """        for (lane, class) in lanes.iter_mut().zip(signature.params.bytes().take(8)) {""",
     LIVE),
    # A generated caller with two arguments swapped: glTexSubImage3D's format and type exchanged.
    ("lnx-gles-A3", "A", "the eleven-argument caller swaps its ninth and tenth arguments",
     SIGNATURES_RS,
     """    f(a[0] as u32, a[1] as u32, a[2] as u32, a[3] as u32, a[4] as u32, a[5] as u32, a[6] as u32, a[7] as u32, a[8] as u32, a[9] as u32, a[10]);""",
     """    f(a[0] as u32, a[1] as u32, a[2] as u32, a[3] as u32, a[4] as u32, a[5] as u32, a[6] as u32, a[7] as u32, a[9] as u32, a[8] as u32, a[10]);""",
     UNIT),
    # A GLint return zero-extended: glGetUniformLocation's -1 ("no such uniform") reaches the guest
    # as 4294967295, a valid-looking location.
    ("lnx-gles-A4", "A", "a signed 32-bit return is zero-extended",
     SIGNATURES_RS,
     """unsafe fn call_s_wp(f: usize, a: &[u64]) -> u64 {
    debug_assert_eq!(a.len(), 2);
    let _ = &a[..2];
    // SAFETY: the caller guarantees `f` is a host function of exactly this prototype, and a
    // function pointer is address-sized on every host this crate builds for.
    let f: extern "C" fn(u32, u64) -> i32 = unsafe { core::mem::transmute::<usize, extern "C" fn(u32, u64) -> i32>(f) };
    let r = f(a[0] as u32, a[1]);
    i64::from(r) as u64""",
     """unsafe fn call_s_wp(f: usize, a: &[u64]) -> u64 {
    debug_assert_eq!(a.len(), 2);
    let _ = &a[..2];
    // SAFETY: the caller guarantees `f` is a host function of exactly this prototype, and a
    // function pointer is address-sized on every host this crate builds for.
    let f: extern "C" fn(u32, u64) -> i32 = unsafe { core::mem::transmute::<usize, extern "C" fn(u32, u64) -> i32>(f) };
    let r = f(a[0] as u32, a[1]);
    u64::from(r as u32)""",
     UNIT),
    # --- values that do not pass through ----------------------------------------------------------
    # glGetString handing the guest the host's own pointer: outside the guest space, so the guest's
    # strlen/strstr imports refuse it.
    ("lnx-gles-A5", "A", "glGetString returns the host's pointer",
     GL_RS,
     """        copy_host_string(gles, c, call, text, name)?
    };
    c.ret().u64(at);""",
     """        copy_host_string(gles, c, call, text, name)?
    };
    let _ = at;
    c.ret().u64(text);""",
     LIVE),
    # Every glGetString copied afresh: a static string with a new address each time.
    ("lnx-gles-B1", "B", "a host string is copied again on every call instead of interned",
     GL_RS,
     """    if let Some(&at) = pool.interned.get(&bytes) {""",
     """    if let Some(&at) = pool.interned.get(&bytes).filter(|_| false) {""",
     LIVE),
    # A mapping unmapped without copying the guest's writes back to the driver.
    ("lnx-gles-A6", "A", "glUnmapBuffer does not copy the shadow back",
     GL_RS,
     """        copy_back(c, &mapping, 0, mapping.length)
    } else {""",
     """        Ok::<(), crate::error::AbiError>(())
    } else {""",
     LIVE),
    # The shadow filled only for GL_MAP_READ_BIT: a write mapping without an invalidate bit then
    # copies a reused shadow's stale bytes back over every byte the guest did not touch.
    ("lnx-gles-A7", "A", "a write-only mapping's shadow is not filled from the buffer",
     GL_RS,
     """    access & MAP_READ != 0 || access & (MAP_INVALIDATE_RANGE | MAP_INVALIDATE_BUFFER) == 0""",
     """    access & MAP_READ != 0""",
     LIVE),
    # Over-correct: the whole shadow copied back at unmap even under GL_MAP_FLUSH_EXPLICIT_BIT, so
    # bytes the guest never flushed reach the buffer.
    ("lnx-gles-B2", "B", "an explicit-flush mapping is copied back whole at unmap",
     GL_RS,
     """    access & MAP_WRITE != 0 && access & MAP_FLUSH_EXPLICIT == 0""",
     """    access & MAP_WRITE != 0""",
     LIVE),
    # --- the mapping's cost (the render thread's 82% in glMapBufferRange) --------------------------
    # Over-correct: every mapping fetched from the buffer, invalidating ones too -- correct bytes,
    # and a read of the driver's (possibly write-combined) memory the specification made unneeded.
    ("lnx-gles-B5", "B", "a write-only invalidating mapping is filled from the buffer anyway",
     GL_RS,
     """    access & MAP_READ != 0 || access & (MAP_INVALIDATE_RANGE | MAP_INVALIDATE_BUFFER) == 0""",
     """    access & MAP_READ != 0 || access & (MAP_INVALIDATE_RANGE | MAP_INVALIDATE_BUFFER) == 0 || true""",
     LIVE),
    # A flush that uploads the whole mapping instead of its own range.
    ("lnx-gles-B6", "B", "glFlushMappedBufferRange uploads the whole mapping",
     GL_RS,
     """            copy_back(c, &mapping, offset, length)?;""",
     """            let _ = (offset, length);
            copy_back(c, &mapping, 0, mapping.length)?;""",
     LIVE),
    # The pool never answers: every map makes a guest mapping again (the measured cost).
    ("lnx-gles-A13", "A", "an idle shadow is never reused",
     GL_RS,
     """    if let Some(shadow) = gles.state().shadows.take(length, |shadow| is_ours(space, shadow)) {""",
     """    if let Some(shadow) = gles.state().shadows.take(length, |shadow| is_ours(space, shadow)).filter(|_| false) {""",
     LIVE),
    # An unmapped mapping's shadow not given back: the pool never fills.
    ("lnx-gles-A14", "A", "glUnmapBuffer does not give the shadow back",
     GL_RS,
     """    release_shadows(gles, [mapping.shadow]);""",
     """    let _ = mapping.shadow;""",
     LIVE),
    # A mapping ended by glDeleteBuffers: its record replaced and its shadow lost with it.
    ("lnx-gles-A15", "A", "a replaced mapping record's shadow is lost",
     GL_RS,
     """    if let Some(old) = replaced {""",
     """    if let Some(old) = replaced.filter(|_| false) {""",
     LIVE),
    # A pooled shadow handed out without asking whether the guest has unmapped it since.
    ("lnx-gles-A16", "A", "an idle shadow the guest unmapped is handed out again",
     GL_RS,
     """            if still_ours(&shadow) {""",
     """            if true || still_ours(&shadow) {""",
     LIVE),
    # --- EGL ----------------------------------------------------------------------------------------
    # eglGetProcAddress answering a thunk for a name the host lacks or no registry has.
    ("lnx-gles-B3", "B", "eglGetProcAddress answers non-NULL for a name the host lacks",
     EGL_RS,
     """        ProcAnswer::NullFromHost | ProcAnswer::NullNotInRegistry | ProcAnswer::NullWithheld => 0,""",
     """        ProcAnswer::NullFromHost | ProcAnswer::NullNotInRegistry | ProcAnswer::NullWithheld => call.address as u64,""",
     LIVE),
    # The Android-only config attribute passed through to a desktop EGL, which rejects the whole
    # request with EGL_BAD_ATTRIBUTE.
    ("lnx-gles-A8", "A", "EGL_RECORDABLE_ANDROID is passed through to the host",
     EGL_RS,
     """    if android.is_empty() {
        return gles.forward(c, call);
    }""",
     """    if android.is_empty() || !android.is_empty() {
        return gles.forward(c, call);
    }""",
     LIVE),
    # EGL_NATIVE_VISUAL_ID answered with the host's X visual id instead of a WINDOW_FORMAT_*.
    ("lnx-gles-A9", "A", "EGL_NATIVE_VISUAL_ID is the host's visual id, untranslated",
     EGL_RS,
     """    let format = android_window_format(red, green, blue, alpha, float);""",
     """    let format = host_visual;""",
     LIVE),
    # A swap that is not counted: FRAMES would read 0 while the engine presents.
    ("lnx-gles-A10", "A", "eglSwapBuffers is not counted as a present",
     EGL_RS,
     """    if r as u32 != 0 {
        gles.note_present();
    }""",
     """    if r as u32 == 0xFFFF_FFFF {
        gles.note_present();
    }""",
     LIVE),
    # The withheld extension shown after all: the engine then asks for coherent mappings.
    ("lnx-gles-A11", "A", "GL_EXT_buffer_storage is not withheld from glGetString",
     GL_RS,
     """                let withheld = super::is_withheld_extension(extension);""",
     """                let withheld = super::is_withheld_extension(extension) && false;""",
     LIVE),
    # glGetStringi's index not remapped past a withheld extension: the two lists disagree.
    ("lnx-gles-A12", "A", "glGetStringi indexes the host's list, withheld entries included",
     GL_RS,
     """            lanes[1] = visible.get(call.lanes[1] as u32 as usize).copied().unwrap_or(u64::from(u32::MAX));""",
     """            let _ = &visible;""",
     LIVE),
    # --- the Windows door ---------------------------------------------------------------------------
    # A Win32 window treated as an X11 one: the refusal that names ANGLE is gone.
    ("lnx-gles-B4", "B", "a Win32 window is given the X11 host row",
     "crates/omni-gfx/src/gles.rs",
     """        RawWindow::Xlib { .. } => Ok(&HOSTS[0]),""",
     """        RawWindow::Xlib { .. } | RawWindow::Win32 { .. } => Ok(&HOSTS[0]),""",
     GFX_UNIT),
]
