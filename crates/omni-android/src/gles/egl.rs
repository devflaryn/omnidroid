//! The EGL calls whose values do not simply pass through. See the module table in [`super`].

use std::ffi::CStr;

use omni_mem::GuestAddr;
use omni_platform::window::RawWindow;

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::{write_return, Call, Gles, ProcAnswer};

/// `EGL_NONE`.
pub const EGL_NONE: i32 = 0x3038;
/// `EGL_EXTENSIONS`, for `eglQueryString`.
pub const EGL_EXTENSIONS: i32 = 0x3055;
/// `EGL_NATIVE_VISUAL_ID`, for `eglGetConfigAttrib`.
pub const EGL_NATIVE_VISUAL_ID: i32 = 0x302E;
/// `EGL_RED_SIZE`.
pub const EGL_RED_SIZE: i32 = 0x3024;
/// `EGL_GREEN_SIZE`.
pub const EGL_GREEN_SIZE: i32 = 0x3023;
/// `EGL_BLUE_SIZE`.
pub const EGL_BLUE_SIZE: i32 = 0x3022;
/// `EGL_ALPHA_SIZE`.
pub const EGL_ALPHA_SIZE: i32 = 0x3021;
/// `EGL_COLOR_COMPONENT_TYPE_EXT` (`EGL_EXT_pixel_format_float`).
pub const EGL_COLOR_COMPONENT_TYPE_EXT: i32 = 0x3339;
/// `EGL_COLOR_COMPONENT_TYPE_FLOAT_EXT`.
pub const EGL_COLOR_COMPONENT_TYPE_FLOAT_EXT: i32 = 0x333B;
/// `EGL_RECORDABLE_ANDROID` (`EGL_ANDROID_recordable`).
pub const EGL_RECORDABLE_ANDROID: i32 = 0x3142;
/// `EGL_FRAMEBUFFER_TARGET_ANDROID` (`EGL_ANDROID_framebuffer_target`).
pub const EGL_FRAMEBUFFER_TARGET_ANDROID: i32 = 0x3147;

/// The Android-only config attributes, and the extension a host must advertise to accept each.
pub const ANDROID_CONFIG_ATTRIBUTES: [(i32, &str, &str); 2] = [
    (EGL_RECORDABLE_ANDROID, "EGL_RECORDABLE_ANDROID", "EGL_ANDROID_recordable"),
    (EGL_FRAMEBUFFER_TARGET_ANDROID, "EGL_FRAMEBUFFER_TARGET_ANDROID", "EGL_ANDROID_framebuffer_target"),
];

/// The most attribute pairs one EGL attribute list is read for. A list is `EGL_NONE`-terminated
/// and the guest controls it, so the walk is bounded; the longest list a config choice can
/// meaningfully have is every config attribute once (EGL 1.5 table 3.4 has 34).
pub const MAX_ATTRIBUTE_PAIRS: usize = 128;

/// `ANativeWindow`'s `WINDOW_FORMAT_*` (and the two `AHARDWAREBUFFER_FORMAT_*` Android reports
/// for 10-bit and half-float window configs) for a config's channel sizes, or 0 when Android has
/// no window format with those channels -- which is what Android's EGL reports for a config that
/// cannot back a window.
#[must_use]
pub fn android_window_format(red: i32, green: i32, blue: i32, alpha: i32, float: bool) -> i32 {
    match (red, green, blue, alpha, float) {
        (8, 8, 8, 8, false) => 1,        // WINDOW_FORMAT_RGBA_8888
        (8, 8, 8, 0, false) => 2,        // WINDOW_FORMAT_RGBX_8888
        (5, 6, 5, 0, false) => 4,        // WINDOW_FORMAT_RGB_565
        (10, 10, 10, 2, false) => 0x2b,  // AHARDWAREBUFFER_FORMAT_R10G10B10A2_UNORM
        (16, 16, 16, 16, true) => 0x16,  // AHARDWAREBUFFER_FORMAT_R16G16B16A16_FLOAT
        _ => 0,
    }
}

/// The OS window behind this guest instance's window source: what `eglGetDisplay` and the
/// host's library choice are made for.
pub(super) fn instance_window(call: &Call) -> AbiResult<RawWindow> {
    let ndk = crate::ndk::active(call.name, call.address)?;
    let Some(source) = ndk.window_source() else {
        return Err(call.refuse(format!(
            "the guest called `{}` and this guest instance has no live window source, so there \
             is no window system to choose a host EGL for and no window to draw into. \
             `Ndk::set_window_source` attaches one (`HostWindowSource::watching` over an \
             `omni_platform::window::Window`)",
            call.name
        )));
    };
    source.raw_window().ok_or_else(|| {
        call.refuse(format!(
            "the guest called `{}` and this instance's window source reports no OS handle \
             ({source:?}); a host EGL display and window surface need a real window",
            call.name
        ))
    })
}

/// A guest `EGLint` attribute list, `EGL_NONE` included, or a refusal naming the argument.
fn read_attributes(
    c: &ImportCall<'_, '_>,
    call: &Call,
    argument: usize,
    at: u64,
) -> AbiResult<Vec<i32>> {
    let mut out = Vec::new();
    let mut cursor = GuestAddr::try_from(at).map_err(|_| call.refuse(format!("{at:#x} is not an address")))?;
    for _ in 0..MAX_ATTRIBUTE_PAIRS {
        let attribute = c.mem().read_i32(cursor, c.blame(argument))?;
        out.push(attribute);
        if attribute == EGL_NONE {
            return Ok(out);
        }
        let value = c.mem().read_i32(cursor + 4, c.blame(argument))?;
        out.push(value);
        cursor += 8;
    }
    Err(call.refuse(format!(
        "the attribute list at {at:#x} has no EGL_NONE within {MAX_ATTRIBUTE_PAIRS} pairs, so it \
         is not an EGL attribute list this layer can pass on"
    )))
}

/// The host display's extension string, read from the host's own memory.
fn display_extensions(gles: &Gles, call: &Call, display: u64) -> AbiResult<String> {
    let text = gles.host_call(call, "eglQueryString", &[display, EGL_EXTENSIONS as u32 as u64])?;
    if text == 0 {
        return Ok(String::new());
    }
    // SAFETY: a non-NULL `eglQueryString` result is a NUL-terminated string the host EGL owns for
    // the life of the display (EGL 1.5 section 3.3); it is read here, on this thread, at once.
    Ok(unsafe { CStr::from_ptr(text as usize as *const core::ffi::c_char) }
        .to_string_lossy()
        .into_owned())
}

fn has_extension(list: &str, name: &str) -> bool {
    list.split_ascii_whitespace().any(|e| e == name)
}

/// `EGLDisplay eglGetDisplay(EGLNativeDisplayType display_id)`
///
/// Android's only native display is `EGL_DEFAULT_DISPLAY`; it stands for "the display the app's
/// windows are on", and on this host that is the display of the window the guest was given.
pub(super) fn get_display(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let native = call.lanes[0];
    if native != 0 {
        return Err(call.refuse(format!(
            "the guest called `eglGetDisplay({native:#x})` from {caller:#x}. An Android app has no \
             native display handle but EGL_DEFAULT_DISPLAY (0) -- there is no Android type it \
             could be -- so this is a value the host EGL would dereference as its own `Display *`",
            caller = call.caller
        )));
    }
    let host = gles.ensure_selected(call)?;
    let window = instance_window(call)?;
    let opened = host.default_display(window)?;
    gles.note(
        "eglGetDisplay",
        format!("EGL_DEFAULT_DISPLAY -> {} = {:#x}", opened.host_call, opened.display),
    );
    c.ret().u64(opened.display);
    Ok(())
}

/// `__eglMustCastToProperFunctionPointerType eglGetProcAddress(const char *procname)`
pub(super) fn get_proc_address(
    gles: &Gles,
    c: &mut ImportCall<'_, '_>,
    call: &Call,
) -> AbiResult<()> {
    let pointer = call.lanes[0];
    if pointer == 0 {
        return Err(call.refuse(format!(
            "the guest called `eglGetProcAddress(NULL)` from {:#x}: there is no name to look up, \
             and NULL is how a caller detects an absent function this call never named",
            call.caller
        )));
    }
    let at = GuestAddr::try_from(pointer).map_err(|_| call.refuse(format!("{pointer:#x} is not an address")))?;
    let name = String::from_utf8_lossy(&c.mem().cstr(at, c.blame(0))?).into_owned();
    let answer = gles.resolve(call, &name)?;
    c.ret().u64(match answer {
        ProcAnswer::Thunk(address) => address as u64,
        ProcAnswer::NullFromHost | ProcAnswer::NullNotInRegistry => 0,
    });
    Ok(())
}

/// `const char *eglQueryString(EGLDisplay dpy, EGLint name)`: the host's text, in guest memory.
pub(super) fn query_string(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let text = gles.forward_value(call)?;
    let at = super::gl::copy_host_string(gles, c, call, text, call.lanes[1] as u32)?;
    c.ret().u64(at);
    Ok(())
}

/// `EGLBoolean eglChooseConfig(EGLDisplay dpy, const EGLint *attrib_list, EGLConfig *configs,
/// EGLint config_size, EGLint *num_config)`
///
/// The Android-only attributes a host without their extensions rejects with `EGL_BAD_ATTRIBUTE`
/// (MEASURED on Mesa 26.0.8's X11 platform: `EGL_RECORDABLE_ANDROID` and
/// `EGL_FRAMEBUFFER_TARGET_ANDROID` both) are **dropped** from the list the host sees, and each
/// drop is recorded. Dropping is the decision because of what the two mean: "a config a
/// `MediaCodec` input surface / the hardware composer's framebuffer target can use". This runtime
/// has neither consumer, so no config on this host can honour them in the sense asked, and every
/// config the host returns without them is as usable as it can be here. A host that advertises the
/// extension gets them unchanged.
pub(super) fn choose_config(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let list_at = call.lanes[1];
    if list_at == 0 {
        return gles.forward(c, call);
    }
    let list = read_attributes(c, call, 1, list_at)?;
    let android: Vec<usize> = (0..list.len() - 1)
        .step_by(2)
        .filter(|&i| ANDROID_CONFIG_ATTRIBUTES.iter().any(|(a, _, _)| *a == list[i]))
        .collect();
    if android.is_empty() {
        return gles.forward(c, call);
    }
    let extensions = display_extensions(gles, call, call.lanes[0])?;
    let mut kept = Vec::with_capacity(list.len());
    let mut i = 0;
    while i < list.len() {
        if list[i] == EGL_NONE {
            kept.push(EGL_NONE);
            break;
        }
        let dropped = ANDROID_CONFIG_ATTRIBUTES
            .iter()
            .find(|(a, _, extension)| *a == list[i] && !has_extension(&extensions, extension));
        match dropped {
            Some((_, spelled, extension)) => gles.note(
                "eglChooseConfig",
                format!(
                    "{spelled} = {value} dropped: the host display does not advertise \
                     {extension}, and a desktop EGL rejects the attribute with EGL_BAD_ATTRIBUTE; \
                     this runtime has no consumer that attribute selects for",
                    value = list[i + 1]
                ),
            ),
            None => kept.extend_from_slice(&list[i..i + 2]),
        }
        i += 2;
    }
    if kept.len() == list.len() {
        return gles.forward(c, call);
    }
    let lanes = [call.lanes[0], kept.as_ptr() as u64, call.lanes[2], call.lanes[3], call.lanes[4]];
    let r = gles.host_call(call, "eglChooseConfig", &lanes)?;
    write_return(c, call.signature, r);
    Ok(())
}

/// `EGLBoolean eglGetConfigAttrib(EGLDisplay dpy, EGLConfig config, EGLint attribute,
/// EGLint *value)`
///
/// `EGL_NATIVE_VISUAL_ID` means something else on Android: the `WINDOW_FORMAT_*` a native window
/// must be given (`ANativeWindow_setBuffersGeometry`) to take this config's frames. The host's value
/// is an X visual id (or a DXGI format, or a `CGLPixelFormat`), which as a window format is
/// meaningless -- so it is **translated**, from the host config's own channel sizes, and recorded.
/// Every other attribute passes through.
pub(super) fn get_config_attrib(
    gles: &Gles,
    c: &mut ImportCall<'_, '_>,
    call: &Call,
) -> AbiResult<()> {
    if call.lanes[2] as u32 as i32 != EGL_NATIVE_VISUAL_ID {
        return gles.forward(c, call);
    }
    let (display, config, out) = (call.lanes[0], call.lanes[1], call.lanes[3]);
    let query = |attribute: i32| -> AbiResult<Option<i32>> {
        let mut value: i32 = 0;
        let ok = gles.host_call(
            call,
            "eglGetConfigAttrib",
            &[display, config, attribute as u32 as u64, &mut value as *mut i32 as u64],
        )?;
        Ok((ok as u32 != 0).then_some(value))
    };
    // The component type first and only when the host has the extension, so that a host without it
    // is never asked an attribute it would answer with EGL_BAD_ATTRIBUTE -- and the queries after
    // it leave the thread's EGL error at EGL_SUCCESS, which is what the guest's call succeeded with.
    let float = if has_extension(&display_extensions(gles, call, display)?, "EGL_EXT_pixel_format_float")
    {
        query(EGL_COLOR_COMPONENT_TYPE_EXT)? == Some(EGL_COLOR_COMPONENT_TYPE_FLOAT_EXT)
    } else {
        false
    };
    let mut sizes = [0i32; 5];
    for (slot, attribute) in
        sizes.iter_mut().zip([EGL_RED_SIZE, EGL_GREEN_SIZE, EGL_BLUE_SIZE, EGL_ALPHA_SIZE, EGL_NATIVE_VISUAL_ID])
    {
        match query(attribute)? {
            Some(value) => *slot = value,
            None => {
                // The host refused the config itself (EGL_BAD_CONFIG, EGL_BAD_DISPLAY): its
                // EGL_FALSE and its error are the guest's answer.
                write_return(c, call.signature, 0);
                return Ok(());
            }
        }
    }
    let [red, green, blue, alpha, host_visual] = sizes;
    let format = android_window_format(red, green, blue, alpha, float);
    let at = GuestAddr::try_from(out).map_err(|_| call.refuse(format!("{out:#x} is not an address")))?;
    c.mem().write_u32(at, format as u32, c.blame(3))?;
    gles.note(
        "eglGetConfigAttrib",
        format!(
            "EGL_NATIVE_VISUAL_ID of a config with R{red}G{green}B{blue}A{alpha}{} (host value \
             {host_visual}) answered {format}{}",
            if float { " float" } else { "" },
            match format {
                1 => " (WINDOW_FORMAT_RGBA_8888)",
                2 => " (WINDOW_FORMAT_RGBX_8888)",
                4 => " (WINDOW_FORMAT_RGB_565)",
                0x2b => " (AHARDWAREBUFFER_FORMAT_R10G10B10A2_UNORM)",
                0x16 => " (AHARDWAREBUFFER_FORMAT_R16G16B16A16_FLOAT)",
                _ => " (no Android window format has these channels)",
            }
        ),
    );
    write_return(c, call.signature, 1);
    Ok(())
}

/// The OS window behind a guest `ANativeWindow *`, checked against this instance's live windows
/// exactly as `vkCreateAndroidSurfaceKHR` checks it.
fn resolve_window(call: &Call, window: u64) -> AbiResult<RawWindow> {
    if window == 0 {
        return Err(call.refuse(format!(
            "the guest called `{}` from {:#x} with `win = NULL`; the window is required",
            call.name, call.caller
        )));
    }
    let at = GuestAddr::try_from(window).map_err(|_| call.refuse(format!("{window:#x} is not an address")))?;
    let ndk = crate::ndk::active(call.name, call.address)?;
    if ndk.window_references(at).is_none() {
        return Err(call.refuse(format!(
            "the guest passed {window:#x} as `{}`'s native window, and that is not a live \
             `ANativeWindow` of this instance -- a handle of another kind, a released window, or \
             a value the engine computed. Making a surface over \"the window\" anyway would put \
             its frames wherever this host's window happens to be",
            call.name
        )));
    }
    instance_window(call)
}

/// `EGLSurface eglCreateWindowSurface(EGLDisplay dpy, EGLConfig config, EGLNativeWindowType win,
/// const EGLint *attrib_list)`
pub(super) fn create_window_surface(
    gles: &Gles,
    c: &mut ImportCall<'_, '_>,
    call: &Call,
) -> AbiResult<()> {
    let (display, config, window, list_at) =
        (call.lanes[0], call.lanes[1], call.lanes[2], call.lanes[3]);
    let raw = resolve_window(call, window)?;
    let attributes = if list_at == 0 { vec![EGL_NONE] } else { read_attributes(c, call, 3, list_at)? };
    let host = gles.ensure_selected(call)?;
    let made = host.create_window_surface(display, config, raw, &attributes)?;
    gles.note(
        "eglCreateWindowSurface",
        format!("ANativeWindow {window:#x} -> {} = {:#x}", made.host_call, made.surface),
    );
    if made.surface != 0 {
        gles.state().window_surfaces.insert(made.surface, display);
    }
    c.ret().u64(made.surface);
    Ok(())
}

/// `EGLBoolean eglDestroySurface(EGLDisplay dpy, EGLSurface surface)`: a window surface goes back
/// through the host that made it (which releases the window); any other surface is forwarded.
pub(super) fn destroy_surface(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let (display, surface) = (call.lanes[0], call.lanes[1]);
    let made_here = gles.state().window_surfaces.remove(&surface);
    let Some(made_on) = made_here else {
        return gles.forward(c, call);
    };
    let host = gles.require_host(call)?;
    let r = host.destroy_window_surface(display, surface)?;
    if r == 0 {
        // The host said no (a wrong display, say): the surface still exists.
        gles.state().window_surfaces.insert(surface, made_on);
    }
    write_return(c, call.signature, u64::from(r));
    Ok(())
}

/// `EGLBoolean eglTerminate(EGLDisplay dpy)`: forwarded; on success the host's window surfaces on
/// that display are gone, and so are their windows' claims.
pub(super) fn terminate(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let r = gles.forward_value(call)?;
    if r as u32 != 0 {
        let display = call.lanes[0];
        gles.state().window_surfaces.retain(|_, on| *on != display);
        if let Some(host) = gles.state().host.clone() {
            host.display_terminated(display);
        }
    }
    write_return(c, call.signature, r);
    Ok(())
}

/// `eglSwapBuffers` and its damage variants: forwarded, and counted when the host says `EGL_TRUE`.
pub(super) fn swap_buffers(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let r = gles.forward_value(call)?;
    if r as u32 != 0 {
        gles.note_present();
    }
    write_return(c, call.signature, r);
    Ok(())
}

/// The calls that take an Android native object this host has no counterpart for: a native
/// display, a native pixmap, an `AHardwareBuffer`, or a platform the guest cannot have chosen.
pub(super) fn refuse_native(_: &Gles, _: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    Err(call.refuse(format!(
        "the guest called `{name}` from {caller:#x} with ({args}). Its native object is an \
         Android one (or a platform an Android app cannot name) that this host has no counterpart \
         for, and passing the value on would have the host EGL dereference it as its own type",
        name = call.name,
        caller = call.caller,
        args = call.lanes[..call.signature.params.len()]
            .iter()
            .map(|lane| format!("{lane:#x}"))
            .collect::<Vec<_>>()
            .join(", "),
    )))
}
