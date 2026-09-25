//! **A `libEGL.so` with no driver behind it** ([`Gles::set_driverless`](super::Gles::set_driverless)):
//! every EGL call answered as Android's own `libEGL` answers it when it has no driver, and every GL
//! call as its no-context hooks answer it.
//!
//! # Why an embedding would want this: measured
//!
//! The headless gate (`tests/gameactivity.rs` without `OMNI_GFX_WINDOW_TESTS`) hands the engine a
//! window and no Vulkan. Once the engine's own flag fetch answers -- it does whenever this host can
//! reach `clientsettingscdn.roblox.com` -- `continueAfterFlagsLoaded_` starts the Lua app, and its
//! `SurfaceController` makes a renderer for that window: `Mode 6 failed: Unable to load Vulkan API`,
//! then OpenGL ES, whose first call is `eglGetDisplay`. With `libEGL.so` unbound, that call killed the
//! engine's **game thread** (thread 6, `android_app_entry`), and the gate's dead-thread assertion
//! failed on all three hosts.
//!
//! # What the engine does with these answers: measured
//!
//! With `eglGetDisplay` answering `EGL_NO_DISPLAY` and `eglGetError` `EGL_BAD_PARAMETER` (2.739.691,
//! Windows, 2026-09-25), the engine made exactly those two calls and took its own failure path:
//!
//! ```text
//! [FLog::SurfaceController] Mode 6 failed: Unable to load Vulkan API
//! [FLog::SurfaceController] Mode 4 failed: Error creating context: eglGetDisplay 300c
//! [FLog::SurfaceController] RenderView is NULL
//! [FLog::SurfaceController] SurfaceController[_:1]::start dataModel(...)
//! ```
//!
//! -- and ran on without a view: the Lua app started, `APP_READY` Home and Landing arrived, and no
//! guest thread died. `0x300c` is the error printed as the engine received it, so the engine reads
//! `eglGetError` after a failed `eglGetDisplay` and says what it read.
//!
//! # Where each answer comes from (AOSP `frameworks/native/opengl/libs/EGL`, read 2026-09-25)
//!
//! | call | answer | source |
//! |---|---|---|
//! | `eglGetDisplay`, `eglGetPlatformDisplay` | `EGL_NO_DISPLAY`, `EGL_BAD_PARAMETER` | `eglApi.cpp`: `if (egl_init_drivers() == EGL_FALSE) return setError(EGL_BAD_PARAMETER, EGL_NO_DISPLAY);` |
//! | `eglGetProcAddress` | `NULL`, `EGL_BAD_PARAMETER` | `eglApi.cpp`, the same check |
//! | `eglBindAPI`, `eglQueryAPI` | `EGL_FALSE`, `EGL_BAD_PARAMETER` | `eglApi.cpp`, the same check |
//! | `eglGetError` | this thread's last error, then `EGL_SUCCESS` | `eglGetErrorImpl`: no `dso`, so `egl_tls_t::getError()`, which clears it |
//! | `eglGetCurrentContext`/`Surface`/`Display` | `EGL_NO_*` | nothing was ever made current |
//! | `eglReleaseThread` | `EGL_TRUE` | `eglReleaseThreadImpl` |
//! | `eglWaitClient`, `eglWaitGL`, `eglWaitNative` | `EGL_FALSE`, `EGL_BAD_CONTEXT` | `if (!cnx->dso) return setError(EGL_BAD_CONTEXT, ...)` |
//! | every other core EGL command (each takes an `EGLDisplay` first) | `0` (`EGL_FALSE`/`EGL_NO_*`/`NULL`), `EGL_BAD_DISPLAY` | `validate_display`/`get_display`: no display was ever made, so every handle is unknown |
//! | every GL command | `0`, nothing done | `gHooksNoContext`: `gl_no_context` returns 0 |
//!
//! `eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS)` is the one exception AOSP makes before any
//! display check (it answers libEGL's own client-extension string); that string is not modelled here,
//! so the call is **refused by name** rather than answered with an invented one.
//!
//! # How faithful it is, said plainly
//!
//! These are the answers AOSP's `libEGL` writes for "no driver". On a current device that branch is
//! not reached with no driver installed at all: `Loader::open` ends such a process with
//! `LOG_ALWAYS_FATAL_IF(!hnd, "couldn't find an OpenGL ES implementation ...")`, and the Android CDD
//! requires OpenGL ES. What a device does reach is `EGL_NO_DISPLAY` from a driver that loaded and
//! could not open its display (`egl_display_t::getPlatformDisplay` returns it after the driver's own
//! `eglGetPlatformDisplay` and `eglGetDisplay` both did, with the driver's error). So the display
//! answer is one a device gives; the error code is libEGL's own no-driver code, and the engine only
//! prints it. Nothing here is a success a device would not give: no display, no context, no surface,
//! no frame.

use omni_mem::GuestAddr;

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::{write_return, Call, Gles, ProcAnswer, ProcRequest, MAX_RECORDS};

/// `EGL_SUCCESS`.
pub const EGL_SUCCESS: i32 = 0x3000;
/// `EGL_BAD_CONTEXT`.
pub const EGL_BAD_CONTEXT: i32 = 0x3006;
/// `EGL_BAD_DISPLAY`.
pub const EGL_BAD_DISPLAY: i32 = 0x3008;
/// `EGL_BAD_PARAMETER`.
pub const EGL_BAD_PARAMETER: i32 = 0x300C;

/// The core EGL 1.0-1.5 commands that do **not** take an `EGLDisplay` as their first argument.
/// Every other core EGL command does (EGL 1.5 section 3; `egl.xml`), and is answered
/// `EGL_BAD_DISPLAY`. `tests.rs` pins this list against the bound table.
pub const WITHOUT_DISPLAY: [&str; 13] = [
    "eglBindAPI",
    "eglGetCurrentContext",
    "eglGetCurrentDisplay",
    "eglGetCurrentSurface",
    "eglGetDisplay",
    "eglGetError",
    "eglGetPlatformDisplay",
    "eglGetProcAddress",
    "eglQueryAPI",
    "eglReleaseThread",
    "eglWaitClient",
    "eglWaitGL",
    "eglWaitNative",
];

thread_local! {
    // EGL's error is per thread (EGL 1.5 section 3.1, `egl_tls_t`), and each guest thread runs on
    // a host thread of its own.
    static ERROR: core::cell::Cell<i32> = const { core::cell::Cell::new(EGL_SUCCESS) };
}

/// What a driverless `libEGL`/`libGLESv2` returns for `name`, and the EGL error it leaves on this
/// thread (`None`: not set -- `eglGetError`, which reads and clears it, and every GL command).
///
/// `Err` is the one call this does not model (the module documentation's exception).
///
/// # Errors
///
/// The reason, for `eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS)`.
pub fn answer(name: &str, lanes: &[u64]) -> Result<(u64, Option<i32>), String> {
    if !name.starts_with("egl") {
        return Ok((0, None));
    }
    Ok(match name {
        "eglGetError" => (u64::from(take_error() as u32), None),
        "eglGetDisplay" | "eglGetPlatformDisplay" | "eglGetProcAddress" | "eglBindAPI" | "eglQueryAPI" => {
            (0, Some(EGL_BAD_PARAMETER))
        }
        "eglGetCurrentContext" | "eglGetCurrentSurface" | "eglGetCurrentDisplay" => (0, Some(EGL_SUCCESS)),
        "eglReleaseThread" => (1, Some(EGL_SUCCESS)),
        "eglWaitClient" | "eglWaitGL" | "eglWaitNative" => (0, Some(EGL_BAD_CONTEXT)),
        "eglQueryString"
            if lanes.first() == Some(&0)
                && lanes.get(1).map(|&v| v as u32 as i32) == Some(super::egl::EGL_EXTENSIONS) =>
        {
            return Err(
                "eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS) asks for libEGL's client-extension \
                 string, which AOSP answers before any driver or display check; this driverless \
                 libEGL does not model that string, and inventing one would advertise extensions \
                 nothing here implements"
                    .to_string(),
            )
        }
        _ => (0, Some(EGL_BAD_DISPLAY)),
    })
}

/// This thread's EGL error, cleared -- `eglGetError`.
pub fn take_error() -> i32 {
    ERROR.with(|cell| cell.replace(EGL_SUCCESS))
}

fn set_error(error: i32) {
    ERROR.with(|cell| cell.set(error));
}

fn error_name(error: i32) -> &'static str {
    match error {
        EGL_SUCCESS => "EGL_SUCCESS",
        EGL_BAD_CONTEXT => "EGL_BAD_CONTEXT",
        EGL_BAD_DISPLAY => "EGL_BAD_DISPLAY",
        EGL_BAD_PARAMETER => "EGL_BAD_PARAMETER",
        _ => "an EGL error",
    }
}

/// The handler every slot goes to while the instance is driverless.
pub(super) fn dispatch(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let n = call.signature.params.len();
    let (value, error) = answer(call.name, &call.lanes[..n]).map_err(|why| call.refuse(why))?;
    if call.name == "eglGetProcAddress" {
        record_request(gles, c, call);
    }
    match error {
        Some(error) => {
            set_error(error);
            gles.note(
                call.name,
                format!("no driver: answered {value:#x}, {} ({error:#x})", error_name(error)),
            );
        }
        None if call.name == "eglGetError" => {}
        None => gles.note(call.name, "no driver: no context is current, so 0 and nothing done".to_string()),
    }
    write_return(c, call.signature, value);
    Ok(())
}

/// An `eglGetProcAddress` asked of a driverless instance, recorded with the others. The name is
/// read only for the record: AOSP answers without reading it, so an unreadable one is not a refusal.
fn record_request(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) {
    let pointer = call.lanes[0];
    let name = GuestAddr::try_from(pointer)
        .ok()
        .filter(|_| pointer != 0)
        .and_then(|at| c.mem().cstr(at, c.blame(0)).ok())
        .map_or_else(|| format!("<name at {pointer:#x}>"), |bytes| String::from_utf8_lossy(&bytes).into_owned());
    let mut state = gles.state();
    if state.requests.len() < MAX_RECORDS {
        state.requests.push(ProcRequest { name, answer: ProcAnswer::NullNoDriver, caller: call.caller });
    } else {
        state.requests_dropped += 1;
    }
}
