//! `ANativeWindow`: the five symbols `libroblox.so` actually imports, and the geometry it has no
//! source for.
//!
//! `ANativeWindow_fromSurface`, `_acquire`, `_release`, `_getWidth`, `_getHeight`.
//!
//! # Five, not nine, and the difference is measured rather than assumed
//!
//! `apk-analysis.md` §4.4 says "ANativeWindow (9)". **That is the count across the whole APK.**
//! MEASURED in `docs/research/apk-undefined-symbols.txt`, per-library section
//! `libroblox.so  (565 undefined; ...)`: that binary's undefined list contains exactly
//! `ANativeWindow_acquire`, `_fromSurface`, `_getHeight`, `_getWidth`, `_release` — five entries,
//! and no other `ANativeWindow_*` line. The other four are somebody else's:
//! `_getFormat` is imported only by `libsurface_util_jni.so`; `_lock`, `_setBuffersGeometry` and
//! `_unlockAndPost` only by `libimage_processing_util_jni.so`. D29 records the same split, from
//! the same file.
//!
//! §8 row 18 lists all eight of `_{getWidth,getHeight,getFormat,setBuffersGeometry,lock,
//! unlockAndPost,acquire,release}` as what `onSurfaceChangedNative` "may call". That row is about
//! the *contract of the Java entry point*, not about what this binary links against — and where
//! the two disagree, the symbol table is the measurement and the row is the summary
//! (`VERIFICATION.md` entry 10). Binding a symbol `libroblox.so` does not import would put a
//! surface here that nothing in this APK can reach, and
//! `every_ndk_symbol_is_an_import_of_the_real_binary_and_outside_the_188` is the test that says so.
//!
//! # Where this sits on the startup path
//!
//! §8 row 17: `onSurfaceCreatedNative(handle, Surface)` releases any old `ANativeWindow`, calls
//! `ANativeWindow_fromSurface(env, surface)`, stores the result at `NativeCode+0x140` and then
//! invokes `callbacks[7] = onNativeWindowCreated(activity, window)`, which makes the glue post
//! `APP_CMD_INIT_WINDOW` down the pipe. §5.2 step 3 has `initializeNativeCode` doing
//! `ALooper_forThread` + `ALooper_acquire`; row 17 is the same shape one object along, which is
//! why the reference count here is the one `looper.rs` already built and not a new idea.
//!
//! # **There is no surface, and the dimensions are therefore the embedding's**
//!
//! Graphics is M6/M7. `omni-gfx` is not wired to any of this, nothing in this runtime has ever
//! produced a pixel, and no `EGLSurface` exists for a window to be the back end of. So the width
//! and the height are **not facts this layer has**; they are facts about the host's output, in
//! exactly the sense D26 gives for `AT_HWCAP` and `ndk::config` gives for the device profile.
//!
//! An instance whose embedding has not called [`Ndk::set_window_geometry`](super::Ndk::set_window_geometry)
//! refuses `ANativeWindow_getWidth` and `_getHeight` **by name**, and the refusal says which call
//! decides. The believable wrong answer is 1920x1080 — a resolution that is plausible, that every
//! caller accepts, and that would be indistinguishable in every log from one the host meant. A
//! device profile nobody chose is worse than a refusal, because the refusal is a line in a log
//! and the profile is a silently different run.
//!
//! # Why the geometry is read at call time rather than copied into the window
//!
//! This is the one place the `AConfiguration` model is deliberately **not** copied.
//! `ndk::config`'s `LiveConfiguration` snapshots the device profile when
//! `AConfiguration_fromAssetManager` fills it, because that is what a device does: a configuration
//! the guest holds does not change under it, which is the reason `onConfigurationChanged` exists.
//! A window is the opposite. `ANativeWindow_getWidth` queries the live surface on a device —
//! AOSP's implementation is `query(NATIVE_WINDOW_WIDTH)` against the producer — so a resize
//! changes what it answers through the *same* `ANativeWindow*`. §8 row 18 is that case by name:
//! `onSurfaceChangedNative` may call `callbacks[8] onNativeWindowResized` without the window
//! handle changing. Snapshotting here would make a resized window keep answering its old size,
//! which is the kind of wrong answer that only shows up as a viewport that is stale by one event.

use omni_mem::GuestAddr;

use crate::abi::Args;
use crate::boundary::{ImportCall, ImportFn};
use crate::error::{AbiError, AbiResult};

use super::{active, Ndk, NdkState};

/// The Java class `ANativeWindow_fromSurface` accepts.
///
/// Declared in `jni::classes` with **no members**, for `android/content/res/AssetManager`'s
/// reason: nothing on the startup path calls a method on a `Surface` from native code, so a
/// member declared here would be a claim about a surface this layer has not measured. It exists
/// so the host can *build* one and so this call can check that what it was given really is one.
pub const SURFACE_CLASS: &str = "android/view/Surface";

/// What the embedding says the window's dimensions are.
///
/// Both fields are `i32` because that is what `ANativeWindow_getWidth` and `_getHeight` return.
/// There is deliberately no `Default`: see this module's documentation for why a number picked
/// here would be a number with nothing behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    /// Pixels across, as `ANativeWindow_getWidth` reports them.
    pub width: i32,
    /// Pixels down, as `ANativeWindow_getHeight` reports them.
    pub height: i32,
}

impl WindowGeometry {
    /// A geometry the embedding has decided.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if either dimension is not positive. A device's surface has a
    /// positive extent in both axes — a zero-width window is the condition
    /// `ANativeWindow_getWidth`'s "negative value on error" exists to report, not a shape a host
    /// can ask for — and accepting `0` here would let a host smuggle in exactly the number this
    /// module refuses to invent, through the one door that is supposed to be a decision.
    pub fn new(width: i32, height: i32) -> AbiResult<WindowGeometry> {
        if width <= 0 || height <= 0 {
            return Err(AbiError::Refused {
                symbol: "WindowGeometry::new".to_string(),
                address: 0,
                why: format!(
                    "a window geometry of {width}x{height} was asked for, and a surface has a \
                     positive extent in both axes. A non-positive dimension is the error \
                     condition `ANativeWindow_getWidth` documents a negative return for, not a \
                     window a host can decide on"
                ),
            });
        }
        Ok(WindowGeometry { width, height })
    }
}

/// One live `ANativeWindow`.
///
/// It holds **no geometry of its own**: see this module's documentation. What it carries is the
/// identity of the Java `Surface` it came from, so that `ANativeWindow_fromSurface` called twice
/// with the same object answers with the same window — which is what a device does, because on a
/// device the `ANativeWindow` *is* the `Surface`'s native peer and `fromSurface` merely takes a
/// reference on it.
#[derive(Debug)]
pub(super) struct LiveWindow {
    /// The `jobject` handle `ANativeWindow_fromSurface` was given.
    pub(super) from_surface: u64,
    /// References, as `ANativeWindow_acquire` and `_release` count them.
    ///
    /// **One at creation**, which is the caller's: the NDK's own documentation for
    /// `ANativeWindow_fromSurface` says it "acquires a reference on the `ANativeWindow` that is
    /// returned; be sure to use `ANativeWindow_release()` when done with it so that it doesn't
    /// leak". §8 row 17 is a caller doing exactly that — it releases the old window before
    /// asking for the new one.
    pub(super) references: i64,
}

impl LiveWindow {
    /// How many references are held.
    #[must_use]
    pub(super) fn references(&self) -> i64 {
        self.references
    }
}

fn refuse(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

fn count(ndk: &Ndk, symbol: &'static str) {
    *ndk.census.lock().entry(symbol).or_insert(0) += 1;
}

/// The guest `ANativeWindow*` as a checked arena address, or a refusal naming the pointer.
///
/// **Checked rather than trusted**, the same as `looper.rs`'s `looper_at`. The arena is divided
/// into a range per handle kind, and `Slots::index_of` refuses a pointer that is inside a range
/// but off a slot boundary — so an `ALooper *` passed where an `ANativeWindow *` belongs, and a
/// `window + 4` the engine computed, are refusals rather than lookups that happen to succeed.
fn window_at(ndk: &Ndk, c: &ImportCall<'_, '_>, window: u64) -> AbiResult<GuestAddr> {
    let state = ndk.state.lock();
    window_in(&state, c, window)
}

/// As [`window_at`], but against a state whose lock the caller **already holds**.
///
/// A liveness check justifies a later `expect` only if the lock was never released in between.
/// `window_at` drops its guard before returning, so a caller that re-locks and writes
/// `.expect("the slot was checked live")` is asserting something a concurrent
/// `ANativeWindow_release` -- taking the last reference and freeing the slot -- can falsify in the
/// gap. `looper.rs`'s `looper_in` carries the full argument; this is the same rule one handle
/// family along.
fn window_in(state: &NdkState, c: &ImportCall<'_, '_>, window: u64) -> AbiResult<GuestAddr> {
    let at = GuestAddr::try_from(window).unwrap_or(0);
    if state.windows.index_of(at).is_none() {
        return Err(refuse(
            c,
            format!(
                "the guest passed {window:#x} as an `ANativeWindow *`, and this instance's windows \
                 live in their own range of its arena. An ANativeWindow is opaque, so a pointer \
                 this layer did not hand out is a handle of another kind, a window from another \
                 instance, or a value the engine computed -- and there is nothing here to operate \
                 on in any of those cases"
            ),
        ));
    }
    if state.windows.get(at).is_none() {
        return Err(refuse(
            c,
            format!(
                "the guest passed {window:#x} as an `ANativeWindow *`, and that slot is not live"
            ),
        ));
    }
    Ok(at)
}

/// `ANativeWindow *ANativeWindow_fromSurface(JNIEnv *env, jobject surface)`
///
/// **The `jobject` is checked against the JNI registry**, exactly as `AAssetManager_fromJava`
/// checks its own argument and for the same reason: accepting any non-null value would turn a
/// wrong argument — an `AssetManager`, a `Configuration`, a stale handle — into a window that
/// answers nonsense thousands of instructions from the mistake. §8 row 17 hands this the `Surface`
/// the Java side passed `onSurfaceCreatedNative`, so a value of another class is a host that built
/// the wrong object, not a window.
///
/// Called twice with the same `Surface` it returns the **same** window and takes a second
/// reference, which is what a device does: the `ANativeWindow` is the `Surface`'s native peer, and
/// `fromSurface` is an `incStrong` on it. Answering a *second* window for one surface would make
/// row 17's "release any old `ANativeWindow`" destroy an object the other holder still has.
///
/// Returns **null** when this instance already holds [`MAX_NATIVE_WINDOWS`](super::MAX_NATIVE_WINDOWS),
/// which is what `fromSurface` answers on a device when it cannot produce one, and what every
/// caller branches on.
///
/// It does **not** need the geometry: nothing on row 17's path asks for a dimension —
/// `callbacks[7] onNativeWindowCreated` takes the window and posts `APP_CMD_INIT_WINDOW`. A
/// refusal here would refuse a call that needs nothing this layer lacks, and would move the
/// diagnosis one call away from the one that actually wanted the number.
fn window_from_surface(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (_env, surface) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ANativeWindow_fromSurface");

    if surface == 0 {
        return Err(refuse(
            c,
            "`ANativeWindow_fromSurface` was given a null `jobject`. jni-surface.md §8 row 17 \
             passes the Surface the Java side handed onSurfaceCreatedNative, so a null here is a \
             host that did not build one"
                .to_string(),
        ));
    }
    // The handle has to be one this runtime's JNI layer issued, and it has to be an
    // `android.view.Surface`. Both checks are the JNI instance's to make.
    let (jni, _thread) = crate::jni::active(c.symbol(), c.address())?;
    let class = jni.instance_class_name(surface).ok_or_else(|| {
        refuse(
            c,
            format!(
                "`ANativeWindow_fromSurface` was given {surface:#x}, which is not a live jobject \
                 of this instance"
            ),
        )
    })?;
    if class != SURFACE_CLASS {
        return Err(refuse(
            c,
            format!(
                "`ANativeWindow_fromSurface` was given an instance of `{class}`. It takes an \
                 `{SURFACE_CLASS}`, and accepting another class would make a wrong argument into \
                 a window that answers nonsense thousands of instructions from the mistake"
            ),
        ));
    }

    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    let existing =
        state.windows.iter().find(|(_, live)| live.from_surface == surface).map(|(at, _)| at);
    if let Some(at) = existing {
        let references = {
            let entry = state.windows.get_mut(at).expect("the slot was just found live");
            entry.references += 1;
            entry.references
        };
        state.record(
            at,
            thread,
            "window",
            format!("fromSurface {surface:#x}: the same window, references now {references}"),
        );
        drop(state);
        c.ret().u64(at as u64);
        return Ok(());
    }
    let Some(at) = state.windows.insert(LiveWindow { from_surface: surface, references: 1 }) else {
        drop(state);
        // **Null, not a refusal.** A device that cannot produce a window answers null here and
        // §8 row 17's caller branches on it; a cap this layer chose is not a reason to invent a
        // failure mode the caller has no arm for.
        c.ret().u64(0);
        return Ok(());
    };
    state.record(
        at,
        thread,
        "window",
        format!("fromSurface {surface:#x}: created at {at:#x}, references now 1"),
    );
    drop(state);
    c.ret().u64(at as u64);
    Ok(())
}

/// `void ANativeWindow_acquire(ANativeWindow *window)`
fn window_acquire(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let window = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ANativeWindow_acquire");
    // **One lock across the check and the increment.** See [`window_in`].
    let mut state = ndk.state.lock();
    let at = window_in(&state, c, window)?;
    let thread = Ndk::thread_index(&mut state);
    let references = {
        let entry = state.windows.get_mut(at).expect("checked live under this same lock");
        entry.references += 1;
        entry.references
    };
    state.record(at, thread, "window", format!("acquire: references now {references}"));
    Ok(())
}

/// `void ANativeWindow_release(ANativeWindow *window)`
///
/// **A release past the last reference is a refusal**, and it arrives as "that slot is not live"
/// from [`window_at`] rather than as a negative count.
///
/// There is **no `references < 0` guard**, and that is a deliberate absence rather than an
/// omission: `VERIFICATION.md` entry 12 records that `ALooper_release` had exactly such a guard
/// and no input could reach it — the count starts at one and the slot is freed the moment it
/// reaches zero, so nothing can observe it below. The statement that the count cannot go negative
/// is a `debug_assert!`, which says *this cannot happen*, rather than an `if` that says *this
/// might*.
///
/// Saturating at zero instead would keep a window alive that guest code believes it has
/// destroyed, and §8 row 17 — which releases the old window before asking for a new one — would
/// then hold a handle to a surface nobody owns.
fn window_release(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let window = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ANativeWindow_release");
    // **One lock across the check and the decrement.** This is the call that frees the slot, so
    // two of these racing is the case [`window_in`] describes.
    let mut state = ndk.state.lock();
    let at = window_in(&state, c, window)?;
    let thread = Ndk::thread_index(&mut state);
    let references = {
        let entry = state.windows.get_mut(at).expect("checked live under this same lock");
        entry.references -= 1;
        entry.references
    };
    debug_assert!(references >= 0, "the slot is freed at zero, so a live window cannot be below it");
    if references == 0 {
        state.windows.remove(at);
        state.record(at, thread, "window", "release: destroyed".to_string());
    } else {
        state.record(at, thread, "window", format!("release: references now {references}"));
    }
    Ok(())
}

/// The decided geometry, or a refusal naming the call that decides.
///
/// # Why this refuses rather than returning the NDK's documented error value
///
/// `android/native_window.h` documents both `ANativeWindow_getWidth` and `_getHeight` as
/// `int32_t` returning a **negative value on error**, and AOSP produces that by passing through
/// the `-errno` from `query(NATIVE_WINDOW_WIDTH)`. So a negative return is available, is
/// in-contract, and is the first thing that comes to mind. It is still the wrong answer here, for
/// three reasons:
///
/// 1. **It is not this condition.** The documented error is a window that *had* dimensions and
///    can no longer be queried — a disconnected or abandoned producer. "Nobody has told this
///    runtime what the host's output is" has no device analogue at all: a real `Surface` that
///    `fromSurface` accepted always has an extent. Reporting a device error for a host
///    configuration gap describes the wrong machine.
/// 2. **The cause would be invisible.** A caller that receives `-19` either takes an error arm
///    this layer cannot see or carries the negative into viewport and framebuffer arithmetic;
///    either way `Ndk::set_window_geometry` was never called and nothing in any log says so. This
///    is `VERIFICATION.md` entry 11's shape one layer up — the run exercises the path and nothing
///    detects the gap. The refusal is the detection, and it names the call.
/// 3. **The project's own line.** `AAssetManager_open` answers null for a missing asset because
///    absence is an ordinary answer the engine probes for; `AConfiguration_fromAssetManager`
///    refuses when the device profile is undecided because that is a host that has not finished
///    configuring the runtime. An undecided window geometry is the second kind, not the first.
///
/// The believable wrong answers, stated so they are on the record: `1920x1080`, which is a device
/// profile nobody chose; and `-1`, which is a device failure that did not happen.
fn decided(ndk: &Ndk, c: &ImportCall<'_, '_>) -> AbiResult<WindowGeometry> {
    ndk.window_geometry().ok_or_else(|| {
        refuse(
            c,
            format!(
                "`{}` was called and this guest instance's window geometry has not been decided. \
                 There is no real surface yet -- graphics is M6/M7 and omni-gfx is not wired to \
                 this -- so the width and the height are facts about the *host's* output, not \
                 facts this layer has. `Ndk::set_window_geometry` is what decides. Answering the \
                 NDK's documented negative-on-error would report a device failure that did not \
                 happen, and answering 1920x1080 would be a device profile nobody chose, \
                 indistinguishable in every log from one the host meant",
                c.symbol()
            ),
        )
    })
}

/// `int32_t ANativeWindow_getWidth(ANativeWindow *window)`
///
/// See [`decided`] for why an undecided geometry refuses rather than returning the negative value
/// `android/native_window.h` documents for failure.
fn window_get_width(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let window = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ANativeWindow_getWidth");
    // The handle is checked **before** the geometry, so a forged pointer is refused as a forged
    // pointer whether or not the host has decided: the two failures have different fixes.
    window_at(&ndk, c, window)?;
    let geometry = decided(&ndk, c)?;
    c.ret().i32(geometry.width);
    Ok(())
}

/// `int32_t ANativeWindow_getHeight(ANativeWindow *window)`
///
/// See [`decided`].
fn window_get_height(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let window = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ANativeWindow_getHeight");
    window_at(&ndk, c, window)?;
    let geometry = decided(&ndk, c)?;
    c.ret().i32(geometry.height);
    Ok(())
}

/// Every `ANativeWindow` symbol. All five are inline: none calls guest code and none changes the
/// address space. `_fromSurface` reaches the JNI registry, which is a lock and not a mapping —
/// the same thing `AAssetManager_fromJava` does from the inline path.
pub(super) static INLINE: &[(&str, ImportFn)] = &[
    ("ANativeWindow_fromSurface", window_from_surface),
    ("ANativeWindow_acquire", window_acquire),
    ("ANativeWindow_release", window_release),
    ("ANativeWindow_getWidth", window_get_width),
    ("ANativeWindow_getHeight", window_get_height),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A geometry is a decision, and a non-positive one is not a decision a host can make.
    #[test]
    fn a_geometry_must_be_positive_in_both_axes() {
        let ok = WindowGeometry::new(1440, 3120).expect("a positive geometry");
        assert_eq!(ok.width, 1440);
        assert_eq!(ok.height, 3120);
        for (width, height) in [(0, 720), (1280, 0), (-1, 720), (1280, -1), (0, 0)] {
            let error = WindowGeometry::new(width, height)
                .expect_err("a non-positive dimension must be refused");
            assert_eq!(error.symbol(), Some("WindowGeometry::new"));
            assert!(error.to_string().contains("positive extent"), "{error}");
        }
    }
}
