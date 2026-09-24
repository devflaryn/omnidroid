//! [`GlesHost`]: the seam between the guest's `libEGL.so`/`libGLESv2.so` and a real host EGL.
//!
//! # Why a trait here, and the implementation in `omni-gfx`
//!
//! [`VulkanHost`](crate::vulkan::VulkanHost)'s argument, unchanged: loading a host library is a
//! `dlopen`/`LoadLibraryExW`, and this crate must build `cfg`-free for five targets with no OS
//! crate in its graph (ARCHITECTURE section 2). So this crate asks and the embedding answers:
//! `omni_gfx::gles::GfxGlesHost` is the implementation this workspace ships, handed over through
//! [`Gles::set_host`](super::Gles::set_host).
//!
//! # What is different from the Vulkan seam, and why it is safe
//!
//! The Vulkan seam carries **no host function pointer at all**: every Vulkan structure is decoded
//! and rebuilt, so the host implementation makes each call itself. GLES cannot be done that way and
//! does not need to be. It is ~350 core entry points and hundreds of extensions whose arguments are
//! integers, floats and pointers into memory the guest owns -- under identity mapping (ARCHITECTURE
//! section 1) there is nothing to decode, only a calling convention to translate. So this seam hands
//! over [`HostProc`]: the **address** of the host's function, which [`super::signatures`]' typed
//! callers then call with the guest's arguments in the host's convention.
//!
//! The property the Vulkan seam protects survives, by construction rather than by type: a host code
//! address must never reach the guest, because the guest would `BLR` into x86-64 machine code with
//! an AAPCS64 frame. [`HostProc`] has no public constructor from an integer that a handler could
//! reach by accident (it is `unsafe`), no `Into<u64>`, and the only thing this crate ever does with
//! one is pass it to a caller in `signatures.rs`. Every address the guest receives -- from an import
//! binding, `eglGetProcAddress` or `dlsym` -- is a thunk slot of this boundary; the live test asserts
//! it (`boundary.symbol_at`).
//!
//! # Which host, chosen by the window at run time
//!
//! [`GlesHost::select`] is given the [`RawWindow`] behind the guest's window and picks the host's
//! EGL for that window system -- `libEGL.so.1` over X11 on Linux, ANGLE on Windows. No `cfg` decides
//! it; the variant does, which is what keeps a Windows or macOS host one match arm away.

use omni_platform::window::RawWindow;

use crate::error::AbiResult;

/// The address of one host EGL or GLES function.
///
/// Constructed only by [`HostProc::new`], which is `unsafe` because the address is about to be
/// called with the prototype the Khronos registry gives the name it was looked up by. See the module
/// documentation for why this type exists at all and what it must never be turned into.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HostProc(usize);

impl HostProc {
    /// Wrap a host function address; `None` for a null one.
    ///
    /// # Safety
    ///
    /// `address` must be the entry point of the host function named by the lookup that produced it,
    /// with the C prototype the Khronos registry (`gl.xml`/`egl.xml`) gives that name, and it must
    /// stay callable for as long as the [`GlesHost`] that returned it lives.
    #[must_use]
    pub unsafe fn new(address: usize) -> Option<Self> {
        (address != 0).then_some(Self(address))
    }

    /// The address, for the typed caller only.
    pub(crate) fn address(self) -> usize {
        self.0
    }
}

impl core::fmt::Debug for HostProc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Printed as a host address on purpose: it is one, it is never a guest value, and a reader
        // of a refusal needs to be able to tell it apart from a thunk address.
        write!(f, "HostProc(host {:#x})", self.0)
    }
}

/// What [`GlesHost::default_display`] made, and the host call it made it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayOpened {
    /// The host `EGLDisplay`, handed to the guest unchanged (identity mapping).
    pub display: u64,
    /// The host's own spelling of the call it made, for the substitution census:
    /// e.g. `eglGetPlatformDisplay(EGL_PLATFORM_X11_KHR, Display* 0x...)`.
    pub host_call: String,
}

/// What [`GlesHost::create_window_surface`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceMade {
    /// The host `EGLSurface`, or `0` (`EGL_NO_SURFACE`) when the **host EGL** refused. A refusal by
    /// the host EGL is an answer the guest is entitled to, and the error code is the host's
    /// `eglGetError`, which the guest's own `eglGetError` then reads -- [`DriverAnswer`]'s argument
    /// (crate::vulkan::DriverAnswer), carried by EGL's own error channel.
    pub surface: u64,
    /// The host call, for the census: e.g. `eglCreatePlatformWindowSurface(Window 0x...)`.
    pub host_call: String,
}

/// A host EGL + GLES implementation for the guest's `libEGL.so` and `libGLESv2.so`.
///
/// Every method may be called from any guest thread. EGL contexts are current **per host thread**,
/// and in this runtime each guest thread runs on its own host thread, so the host's own per-thread
/// state is the guest thread's.
pub trait GlesHost: Send + Sync + core::fmt::Debug {
    /// Load the host EGL and GLES for the window system `window` belongs to, and describe what was
    /// loaded (`"libEGL.so.1 + libGLESv2.so.2, EGL_PLATFORM_X11_KHR"`).
    ///
    /// Idempotent for one window system. **A second, different system is refused**: one process's
    /// guest has one set of entry points, and handing it a second library's functions for the same
    /// names would make two thunks of one name call two drivers.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) naming what is missing -- a window system this
    /// host has no EGL for (naming exactly what would plug it in), or a library that is not
    /// installed.
    fn select(&self, window: RawWindow) -> AbiResult<String>;

    /// The host's function for `name`, or `None` when the host does not have it.
    ///
    /// The host's truth: its libraries' exports first, then its own `eglGetProcAddress`. EGL 1.5
    /// section 3.10 allows a non-NULL `eglGetProcAddress` for a name the implementation does not
    /// support (GLVND answers every `gl*` name with a dispatch stub), so a `Some` for an extension
    /// name is what the host said, not a proof of support -- the specification's own instruction is
    /// to check the extension string, which the guest's `glGetString` returns unaltered.
    ///
    /// # Errors
    ///
    /// Refused when [`select`](GlesHost::select) has not succeeded: no library is loaded to ask.
    fn proc_address(&self, name: &str) -> AbiResult<Option<HostProc>>;

    /// The host `EGLDisplay` that stands for Android's `EGL_DEFAULT_DISPLAY` on this window's
    /// system: the display of the window the guest will draw into.
    ///
    /// # Errors
    ///
    /// Refused when the host has no display for that system; an `EGL_NO_DISPLAY` from the host EGL
    /// is also a refusal, naming the host's `eglGetError`, because Android's `eglGetDisplay` of the
    /// default display does not fail and the guest has no branch that would make sense of it.
    fn default_display(&self, window: RawWindow) -> AbiResult<DisplayOpened>;

    /// `eglCreateWindowSurface`'s host half: an `EGLSurface` over the host window behind the
    /// guest's `ANativeWindow`, with `attributes` (an `EGL_NONE`-terminated `EGLint` list) passed
    /// through.
    ///
    /// # Errors
    ///
    /// Refused when the window belongs to another system than [`select`](GlesHost::select) chose,
    /// or when another renderer already owns the window (`omni_gfx::claim`).
    fn create_window_surface(
        &self,
        display: u64,
        config: u64,
        window: RawWindow,
        attributes: &[i32],
    ) -> AbiResult<SurfaceMade>;

    /// `eglDestroySurface` for a surface [`create_window_surface`](GlesHost::create_window_surface)
    /// made: the host's `EGLBoolean`, and the window released for another owner.
    ///
    /// # Errors
    ///
    /// Refused when `surface` is not one this host made.
    fn destroy_window_surface(&self, display: u64, surface: u64) -> AbiResult<u32>;

    /// `eglTerminate(display)` succeeded: every window surface made on it is gone with it, so the
    /// host releases their windows.
    fn display_terminated(&self, display: u64);
}
