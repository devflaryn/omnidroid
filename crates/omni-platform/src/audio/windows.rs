//! Windows backend for the audio seam: WASAPI, shared mode, event-driven.
//!
//! # What `open` does, in order
//!
//! 1. `CoIncrementMTAUsage`, held for the life of the stream, and
//!    `CoInitializeEx(COINIT_MULTITHREADED)`, held only for the length of `open`. See "COM, and
//!    which thread it is on" below.
//! 2. `CoCreateInstance(CLSID_MMDeviceEnumerator)`, `GetDefaultAudioEndpoint(eRender, eConsole)`,
//!    `IMMDevice::Activate(IID_IAudioClient)`.
//! 3. `IAudioClient::GetMixFormat`, read by [`parse_mix_format`] and freed with `CoTaskMemFree`.
//! 4. `IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
//!    duration, 0, mix format, null)` — the mix format handed straight back, so the engine is
//!    asked to convert nothing.
//! 5. `GetBufferSize`, `GetDevicePeriod`, `CreateEventW` + `SetEventHandle`, and
//!    `GetService(IID_IAudioRenderClient)`.
//!
//! # COM without a COM crate
//!
//! `windows-sys` has WASAPI's GUIDs, structures and constants and, deliberately, no COM method
//! bindings; the `windows` crate that has them would be a second Windows binding crate in the
//! workspace for the sake of thirteen calls. So this file declares the vtables itself, as
//! `#[repr(C)]` structs holding the slots **up to the last one it calls** — a prefix of the real
//! table, which is all a reader of the table needs, since nothing here ever builds one.
//!
//! **A wrong slot is a silent crash**, or worse, a call to a different method whose arguments
//! happen to fit, so the order is not from memory. Each struct was transcribed from the C-interface
//! `…Vtbl` structs in the Windows SDK 10.0.26100.0 headers `mmdeviceapi.h` and `Audioclient.h`,
//! cross-checked against the `windows` 0.61.3 crate's generated `…_Vtbl` structs (the two agree),
//! and `vtable_slots_are_where_the_sdk_puts_them` pins every called slot to its index in the
//! header — so a placeholder lost in an edit fails a unit test rather than a live stream. The three
//! interface IDs `windows-sys` does not carry were taken from the same headers' `MIDL_INTERFACE`
//! strings.
//!
//! Every interface pointer is owned by a [`Com`], which releases it exactly once.
//!
//! # COM, and which thread it is on
//!
//! The stream is `Send`: it is opened on one thread and driven — and dropped — on another. COM's
//! rules are per thread, so what that requires had to be decided rather than inherited.
//!
//! * **The multithreaded apartment is pinned for the stream's life** with `CoIncrementMTAUsage`,
//!   and released with `CoDecrementMTAUsage` when the stream drops, on whichever thread that is:
//!   the usage count is a cookie rather than a per-thread initialisation, which is what the call
//!   exists for. While the MTA exists, a thread that never initialised COM is in the *implicit*
//!   MTA and may call MTA objects, so the thread that drives the stream needs no COM setup of its
//!   own, and the objects outlive the thread that created them.
//! * **`CoInitializeEx(COINIT_MULTITHREADED)` is called on the opening thread and balanced before
//!   `open` returns**, so that `open` leaves that thread's COM state as it found it. `S_OK` and
//!   `S_FALSE` (the thread was already in the MTA) are both balanced with `CoUninitialize` —
//!   `S_FALSE` is a reference too. Never balancing would leak one initialisation into whatever
//!   thread the embedder happened to open audio on; balancing in `Drop` is not possible at all,
//!   because `Drop` may run on another thread and `CoUninitialize` belongs to the thread that
//!   initialised.
//! * **`RPC_E_CHANGED_MODE` is tolerated.** It means the opening thread is already a
//!   single-threaded apartment, which is not this seam's to change. The objects are then created in
//!   that STA and later called from another thread without marshalling. MEASURED on this host: the
//!   enumerator's class is registered `ThreadingModel = both`
//!   (`HKCR\CLSID\{BCDE0395-E52F-467C-8E3D-C4579291692E}\InprocServer32`), so it is created in the
//!   caller's apartment with no proxy in between. That calling it from another thread is then
//!   safe is ASSUMED — it is the free-threaded behaviour "both" advertises — and **not measured**:
//!   no test here opens from an STA. The runtime opens audio from threads it started, which have no
//!   apartment, so it takes the `S_OK` path.

use core::ffi::c_void;
use core::num::{NonZeroU16, NonZeroU32};
use core::ptr::{self, NonNull};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_NOT_FOUND, GetLastError, HANDLE, RPC_E_CHANGED_MODE, S_FALSE, S_OK, WAIT_FAILED,
};
use windows_sys::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, EDataFlow,
    ERole, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows_sys::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows_sys::Win32::Media::Multimedia::{
    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT,
};
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, CO_MTA_USAGE_COOKIE, COINIT_MULTITHREADED, CoCreateInstance, CoDecrementMTAUsage,
    CoIncrementMTAUsage, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows_sys::core::{GUID, HRESULT};

use super::{AudioError, AudioResult, OutputFormat};

// ---------------------------------------------------------------------------------------------
// Interface IDs and vtables
// ---------------------------------------------------------------------------------------------

/// `IID_IMMDeviceEnumerator`, `MIDL_INTERFACE("A95664D2-9614-4F35-A746-DE8DB63617E6")` in
/// `mmdeviceapi.h`. The SDK's spelling is kept so that the name can be searched for.
#[allow(non_upper_case_globals)]
const IID_IMMDeviceEnumerator: GUID = GUID::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);

/// `IID_IAudioClient`, `MIDL_INTERFACE("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2")` in
/// `Audioclient.h`.
#[allow(non_upper_case_globals)]
const IID_IAudioClient: GUID = GUID::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);

/// `IID_IAudioRenderClient`, `MIDL_INTERFACE("F294ACFC-3146-4483-A7BF-ADDCA7C260E2")` in
/// `Audioclient.h`.
#[allow(non_upper_case_globals)]
const IID_IAudioRenderClient: GUID = GUID::from_u128(0xf294acfc_3146_4483_a7bf_addca7c260e2);

/// `E_NOTFOUND` as `mmdeviceapi.h` defines it: `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`, `0x80070490`.
///
/// **Not** `windows_sys::Win32::Data::HtmlHelp::E_NOTFOUND`, which is `0x8000100D` — a different
/// code from a different header under the same name. Matching that one would make
/// [`AudioError::NoDevice`] unreachable and nothing would say so.
const E_NOTFOUND: HRESULT = hresult_from_win32(ERROR_NOT_FOUND);

/// A vtable slot this backend never calls, present only so that the slots after it sit at the
/// offsets the SDK puts them at. Pointer-sized, as every vtable slot is.
type Unused = *const c_void;

/// `IUnknownVtbl`: the three slots every COM vtable begins with.
#[repr(C)]
struct IUnknownVtbl {
    _query_interface: Unused,
    _add_ref: Unused,
    release: unsafe extern "system" fn(this: *mut c_void) -> u32,
}

/// `IMMDeviceEnumeratorVtbl` in `mmdeviceapi.h`, up to `GetDefaultAudioEndpoint`.
#[repr(C)]
struct IMMDeviceEnumeratorVtbl {
    _base: IUnknownVtbl,
    _enum_audio_endpoints: Unused,
    get_default_audio_endpoint: unsafe extern "system" fn(
        this: *mut c_void,
        flow: EDataFlow,
        role: ERole,
        endpoint: *mut *mut c_void,
    ) -> HRESULT,
}

/// `IMMDeviceVtbl` in `mmdeviceapi.h`, up to `Activate`.
#[repr(C)]
struct IMMDeviceVtbl {
    _base: IUnknownVtbl,
    /// `pActivationParams` is a `PROPVARIANT*`; it is always null here, so it is declared as an
    /// untyped pointer rather than pulling in `Win32_System_Com_StructuredStorage` for its type.
    activate: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        context: u32,
        params: *const c_void,
        interface: *mut *mut c_void,
    ) -> HRESULT,
}

/// `IAudioClientVtbl` in `Audioclient.h`, whole: `GetService` is its last slot.
#[repr(C)]
struct IAudioClientVtbl {
    _base: IUnknownVtbl,
    initialize: unsafe extern "system" fn(
        this: *mut c_void,
        share_mode: AUDCLNT_SHAREMODE,
        stream_flags: u32,
        buffer_duration: i64,
        periodicity: i64,
        format: *const WAVEFORMATEX,
        session: *const GUID,
    ) -> HRESULT,
    get_buffer_size: unsafe extern "system" fn(this: *mut c_void, frames: *mut u32) -> HRESULT,
    _get_stream_latency: Unused,
    get_current_padding: unsafe extern "system" fn(this: *mut c_void, frames: *mut u32) -> HRESULT,
    _is_format_supported: Unused,
    get_mix_format:
        unsafe extern "system" fn(this: *mut c_void, format: *mut *mut WAVEFORMATEX) -> HRESULT,
    get_device_period: unsafe extern "system" fn(
        this: *mut c_void,
        default_period: *mut i64,
        minimum_period: *mut i64,
    ) -> HRESULT,
    start: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    stop: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    _reset: Unused,
    set_event_handle: unsafe extern "system" fn(this: *mut c_void, event: HANDLE) -> HRESULT,
    get_service: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        service: *mut *mut c_void,
    ) -> HRESULT,
}

/// `IAudioRenderClientVtbl` in `Audioclient.h`, whole.
#[repr(C)]
struct IAudioRenderClientVtbl {
    _base: IUnknownVtbl,
    get_buffer:
        unsafe extern "system" fn(this: *mut c_void, frames: u32, data: *mut *mut u8) -> HRESULT,
    release_buffer:
        unsafe extern "system" fn(this: *mut c_void, frames: u32, flags: u32) -> HRESULT,
}

/// An owned reference to a COM interface whose vtable begins with `V`'s slots.
///
/// A COM interface pointer is the address of an object whose first word is the address of its
/// vtable, so `this` is typed as a pointer to that word.
struct Com<V> {
    this: NonNull<*const V>,
}

impl<V> Com<V> {
    /// Take ownership of one reference, or `None` for null.
    ///
    /// # Safety
    ///
    /// `raw` must be null, or an interface pointer whose vtable begins with `V`'s slots and of
    /// which the caller owns one reference. That reference passes to the returned value.
    unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw.cast::<*const V>()).map(|this| Com { this })
    }

    /// The interface pointer, for passing as a method's `This`.
    fn as_raw(&self) -> *mut c_void {
        self.this.as_ptr().cast()
    }

    /// The vtable.
    fn vtbl(&self) -> &V {
        // SAFETY: by `from_raw`'s contract `this` is a live COM object whose first word points at
        // a vtable beginning with `V`, and it stays live while `self` holds its reference. A COM
        // object's vtable neither moves nor changes while the object lives.
        unsafe { &**self.this.as_ptr() }
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // SAFETY: every COM vtable begins with `IUnknown`'s three slots, whatever follows them, so
        // the object's first word points at something with an `IUnknownVtbl` prefix whatever `V`
        // is. This value owns exactly one reference (`from_raw`'s contract) and gives it up
        // exactly once, here. The returned count is advisory and is ignored, as COM says to.
        unsafe {
            let vtbl = *self.this.as_ptr().cast::<*const IUnknownVtbl>();
            ((*vtbl).release)(self.as_raw());
        }
    }
}

/// Own the interface a COM call handed back through its out-pointer, or return the call's failing
/// `HRESULT`.
///
/// The `HRESULT` goes back to the caller to name, because what a failure *means* depends on the
/// call — `GetDefaultAudioEndpoint`'s `E_NOTFOUND` is [`AudioError::NoDevice`].
///
/// # Safety
///
/// `hr` and `raw` must be the result and the out-pointer of one COM call that, when it succeeds,
/// stores an owned reference to an interface whose vtable begins with `V`'s slots — the IID the
/// call asked for must be `V`'s.
unsafe fn take<V>(hr: HRESULT, raw: *mut c_void) -> Result<Com<V>, HRESULT> {
    if hr < 0 {
        return Err(hr);
    }
    // SAFETY: the call succeeded, so by this function's contract `raw` is an owned `V` (or null,
    // which COM rules out for a successful call and which `from_raw` refuses rather than trusts).
    let com = unsafe { Com::from_raw(raw) };
    Ok(com.expect("a COM call reported success and returned a null interface pointer"))
}

/// A failing `HRESULT` as an [`AudioError::Os`]; a succeeding one (`S_OK`, `S_FALSE`) as `Ok`.
fn check(operation: &'static str, api: &'static str, hr: HRESULT) -> AudioResult<()> {
    if hr < 0 {
        return Err(AudioError::Os { operation, api, code: hr });
    }
    Ok(())
}

/// `HRESULT_FROM_WIN32` from `winerror.h`, transcribed: a code that is already zero or negative as
/// an `HRESULT` passes through, and any other becomes `0x8007xxxx` (severity error,
/// `FACILITY_WIN32`, the low 16 bits of the code).
const fn hresult_from_win32(code: u32) -> HRESULT {
    let as_hresult = code as HRESULT;
    if as_hresult <= 0 {
        as_hresult
    } else {
        ((code & 0xFFFF) | (7 << 16) | 0x8000_0000) as HRESULT
    }
}

/// `GetLastError` as an [`AudioError::Os`], on the `HRESULT` scale the variant documents.
fn last_error(operation: &'static str, api: &'static str) -> AudioError {
    // SAFETY: no arguments and no memory; the caller calls this straight after the failing API.
    let code = unsafe { GetLastError() };
    AudioError::Os { operation, api, code: hresult_from_win32(code) }
}

/// What `GetDefaultAudioEndpoint` failing with `code` means.
fn endpoint_error(code: HRESULT) -> AudioError {
    if code == E_NOTFOUND {
        return AudioError::NoDevice { operation: "open" };
    }
    AudioError::Os { operation: "open", api: "IMMDeviceEnumerator::GetDefaultAudioEndpoint", code }
}

// ---------------------------------------------------------------------------------------------
// COM lifetime
// ---------------------------------------------------------------------------------------------

/// One `CoIncrementMTAUsage`, undone when dropped. See this module's "COM, and which thread it is
/// on".
struct MtaUsage(CO_MTA_USAGE_COOKIE);

impl MtaUsage {
    fn acquire() -> AudioResult<Self> {
        let mut cookie: CO_MTA_USAGE_COOKIE = ptr::null_mut();
        // SAFETY: writes one cookie at the pointer, which is live for the call.
        check("open", "CoIncrementMTAUsage", unsafe { CoIncrementMTAUsage(&raw mut cookie) })?;
        Ok(MtaUsage(cookie))
    }
}

impl Drop for MtaUsage {
    fn drop(&mut self) {
        // SAFETY: the cookie came from a successful `CoIncrementMTAUsage` and is given back exactly
        // once. Declared last in `AudioOutput`, so every COM object has been released by now. The
        // result is ignored: `Drop` has nobody to report it to.
        unsafe { CoDecrementMTAUsage(self.0) };
    }
}

/// This thread's `CoInitializeEx`, held for the length of `open` and balanced when dropped.
struct ThreadCom {
    /// False for `RPC_E_CHANGED_MODE`, which took no reference and must not be balanced.
    balance: bool,
}

impl ThreadCom {
    fn enter() -> AudioResult<Self> {
        // SAFETY: the reserved argument is null, as required, and the flag is a constant.
        let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED as u32) };
        match hr {
            S_OK | S_FALSE => Ok(ThreadCom { balance: true }),
            RPC_E_CHANGED_MODE => Ok(ThreadCom { balance: false }),
            code => Err(AudioError::Os { operation: "open", api: "CoInitializeEx", code }),
        }
    }
}

impl Drop for ThreadCom {
    fn drop(&mut self) {
        if self.balance {
            // SAFETY: balances the successful `CoInitializeEx` in `enter`, on the same thread —
            // this value never leaves `open`'s stack frame.
            unsafe { CoUninitialize() };
        }
    }
}

/// A `CoTaskMemAlloc`'d mix format, freed when dropped.
struct MixFormat(NonNull<WAVEFORMATEX>);

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `GetMixFormat`, which allocates with
        // `CoTaskMemAlloc` and hands ownership to the caller; it is freed exactly once, here.
        unsafe { CoTaskMemFree(self.0.as_ptr().cast_const().cast()) };
    }
}

// ---------------------------------------------------------------------------------------------
// The pure parts: the mix format and the unit conversions
// ---------------------------------------------------------------------------------------------

/// A mix format this seam can write: interleaved 32-bit float, with neither count zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Float32Format {
    rate: NonZeroU32,
    channels: NonZeroU16,
}

impl Float32Format {
    fn output(self) -> OutputFormat {
        OutputFormat { sample_rate: self.rate.get(), channels: self.channels.get() }
    }
}

/// View a mix format as the bytes the host allocated for it: the 18-byte `WAVEFORMATEX` header and
/// the `cbSize` bytes after it.
///
/// This is the one way a mix format becomes bytes — [`AudioOutput::open`] uses it on what
/// `GetMixFormat` returned and the unit tests use it on formats laid out in the SDK's own
/// structures — so the tests feed [`parse_mix_format`] exactly what the real caller does
/// (VERIFICATION entry 20).
///
/// # Safety
///
/// `format` must point at a `WAVEFORMATEX` followed by `cbSize` readable bytes, all live and
/// unmodified for `'a`.
unsafe fn format_bytes<'a>(format: NonNull<WAVEFORMATEX>) -> &'a [u8] {
    // SAFETY: the header is readable (this function's contract). `WAVEFORMATEX` is `packed(1)`,
    // so the field read needs no alignment, and it is a copy rather than a reference.
    let extra = unsafe { (*format.as_ptr()).cbSize };
    let len = size_of::<WAVEFORMATEX>() + usize::from(extra);
    // SAFETY: `len` bytes are readable and live for `'a`, by this function's contract.
    unsafe { core::slice::from_raw_parts(format.as_ptr().cast::<u8>(), len) }
}

/// What the device's mix format is, or why this seam will not write it.
///
/// Accepted: `WAVE_FORMAT_IEEE_FLOAT`, or `WAVE_FORMAT_EXTENSIBLE` whose `SubFormat` is
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` — in either case with 32 bits per sample, at least one
/// channel, a non-zero rate, and a block alignment of exactly four bytes per channel. The last
/// is what [`AudioOutput::write`]'s copy is sized by, so it is checked rather than assumed.
/// Everything else is [`AudioError::FormatNotFloat`] naming the tag and the bits.
///
/// `wValidBitsPerSample` and `dwChannelMask` are not consulted: the first is meaningless for a
/// float container, and the second says which speakers the channels are, which is the caller's
/// business and not a reason to refuse.
///
/// # Panics
///
/// When `bytes` is shorter than a `WAVEFORMATEX` header, which [`format_bytes`] never produces.
/// The check is what makes the header read sound, so it is an `assert!` and not a `debug_assert!`.
fn parse_mix_format(bytes: &[u8], operation: &'static str) -> AudioResult<Float32Format> {
    assert!(
        bytes.len() >= size_of::<WAVEFORMATEX>(),
        "a mix format is at least its {}-byte header; this one is {} bytes",
        size_of::<WAVEFORMATEX>(),
        bytes.len()
    );
    // SAFETY: at least `size_of::<WAVEFORMATEX>()` bytes are readable (the assert above), the read
    // is unaligned, and every bit pattern is a valid `WAVEFORMATEX` — it is seven integers.
    let header = unsafe { bytes.as_ptr().cast::<WAVEFORMATEX>().read_unaligned() };
    let (tag, bits) = (header.wFormatTag, header.wBitsPerSample);

    let float = match u32::from(tag) {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE if bytes.len() >= size_of::<WAVEFORMATEXTENSIBLE>() => {
            // SAFETY: the guard guarantees the bytes, the read is unaligned, and every bit pattern
            // is a valid `WAVEFORMATEXTENSIBLE`: a header of integers, a union of `u16`s, a `u32`
            // and a GUID of integers.
            let extensible =
                unsafe { bytes.as_ptr().cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            same_guid(extensible.SubFormat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
        }
        // An extensible header too short to carry its `SubFormat` falls here with every other
        // tag: its sample type cannot be known, so it is not known to be float.
        _ => false,
    };

    let refused = AudioError::FormatNotFloat { operation, tag, bits };
    let (Some(rate), Some(channels)) =
        (NonZeroU32::new(header.nSamplesPerSec), NonZeroU16::new(header.nChannels))
    else {
        return Err(refused);
    };
    let packed = u32::from(header.nBlockAlign) == 4 * u32::from(channels.get());
    if !(float && bits == 32 && packed) {
        return Err(refused);
    }
    Ok(Float32Format { rate, channels })
}

/// GUID equality; `windows-sys`'s `GUID` does not implement `PartialEq`.
fn same_guid(a: GUID, b: GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

/// `REFERENCE_TIME` units — 100 ns — per second.
const HNS_PER_SECOND: u64 = 10_000_000;

/// `frames` at `rate` as a `REFERENCE_TIME`, **rounded up**, so that a buffer asked for in frames
/// is never asked for as a duration a fraction of a frame short of it.
fn hns_from_frames(frames: u32, rate: NonZeroU32) -> i64 {
    let hns = (u64::from(frames) * HNS_PER_SECOND).div_ceil(u64::from(rate.get()));
    // `u32::MAX` frames at 1 Hz is 4.3e16 units; `i64::MAX` is 9.2e18.
    i64::try_from(hns).expect("u32::MAX frames at 1 Hz fits in an i64 of 100 ns units")
}

/// A `REFERENCE_TIME` as frames at `rate`, rounded to the nearest frame. A negative duration is
/// zero frames, and one too long for a `u32` saturates.
fn frames_from_hns(hns: i64, rate: NonZeroU32) -> u32 {
    let hns = u128::from(u64::try_from(hns).unwrap_or(0));
    let per_second = u128::from(HNS_PER_SECOND);
    let frames = (hns * u128::from(rate.get()) + per_second / 2) / per_second;
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// A timeout as `WaitForSingleObject` milliseconds: **rounded up**, so that a wait asked for in
/// microseconds does not silently become a zero-length poll, and capped one short of `INFINITE`,
/// so that no finite timeout, however long, becomes a wait that never ends.
fn wait_millis(timeout: Duration) -> u32 {
    let millis = timeout.as_nanos().div_ceil(1_000_000);
    u32::try_from(millis).unwrap_or(u32::MAX).min(INFINITE - 1)
}

// ---------------------------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------------------------

/// A WASAPI shared-mode render stream.
///
/// **Field order is drop order, and it is load-bearing**: the render client before the audio
/// client it came from, the event handle only after the client that signals it is gone, and the
/// MTA usage last, after every COM object.
pub(super) struct AudioOutput {
    render: Com<IAudioRenderClientVtbl>,
    client: Com<IAudioClientVtbl>,
    /// Held although nothing calls it after `open`: no document says an audio client outlives the
    /// last reference to the device it was activated from, and holding one pointer costs nothing.
    _device: Com<IMMDeviceVtbl>,
    /// Signalled by the engine each time it has consumed a period. Auto-reset, as Microsoft's
    /// render example makes it: one signal wakes one wait.
    event: OwnedHandle,
    format: Float32Format,
    buffer_frames: u32,
    period_frames: u32,
    /// Whether `Start` has succeeded without a `Stop` since. `Start` on a started client fails with
    /// `AUDCLNT_E_NOT_STOPPED`, and `Drop` must know whether there is anything to stop.
    running: bool,
    _mta: MtaUsage,
}

// SAFETY: see "COM, and which thread it is on" in this module's header. The COM objects live in
// the MTA (or, when opened from an STA, are "both"-model objects created without a proxy), and
// `_mta` keeps the MTA alive for exactly as long as this value exists, so any thread holding it is
// at least in the implicit MTA and may call them. The event is a kernel handle, which any thread
// may wait on or close. Not `Sync`, and the raw pointers keep it so: `write` pairs `GetBuffer`
// with `ReleaseBuffer`, and `&mut self` serialises that only if the value cannot be shared.
unsafe impl Send for AudioOutput {}

impl AudioOutput {
    pub(super) fn open(buffer_frames: u32) -> AudioResult<Self> {
        let mta = MtaUsage::acquire()?;
        let _thread = ThreadCom::enter()?;

        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: both GUIDs are statics, the outer pointer is null (no aggregation), and the
        // out-pointer is live for the call.
        let hr = unsafe {
            CoCreateInstance(
                &MMDeviceEnumerator,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMMDeviceEnumerator,
                &raw mut raw,
            )
        };
        // SAFETY: that call, asked for `IID_IMMDeviceEnumerator`, stores an owned
        // `IMMDeviceEnumerator` when it succeeds.
        let enumerator = unsafe { take::<IMMDeviceEnumeratorVtbl>(hr, raw) }.map_err(|code| {
            AudioError::Os { operation: "open", api: "CoCreateInstance(MMDeviceEnumerator)", code }
        })?;

        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: a live enumerator, two enum constants, and an out-pointer live for the call. The
        // slot is `GetDefaultAudioEndpoint`'s; see "COM without a COM crate".
        let hr = unsafe {
            (enumerator.vtbl().get_default_audio_endpoint)(
                enumerator.as_raw(),
                eRender,
                eConsole,
                &raw mut raw,
            )
        };
        // SAFETY: `GetDefaultAudioEndpoint` stores an owned `IMMDevice` when it succeeds.
        let device = unsafe { take::<IMMDeviceVtbl>(hr, raw) }.map_err(endpoint_error)?;

        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: a live device, a static IID, no activation parameters (the header marks them
        // `_In_opt_`), and an out-pointer live for the call.
        let hr = unsafe {
            (device.vtbl().activate)(
                device.as_raw(),
                &IID_IAudioClient,
                CLSCTX_ALL,
                ptr::null(),
                &raw mut raw,
            )
        };
        // SAFETY: `Activate`, asked for `IID_IAudioClient`, stores an owned `IAudioClient` when it
        // succeeds.
        let client = unsafe { take::<IAudioClientVtbl>(hr, raw) }.map_err(|code| {
            AudioError::Os { operation: "open", api: "IMMDevice::Activate", code }
        })?;

        let mut raw_mix: *mut WAVEFORMATEX = ptr::null_mut();
        // SAFETY: a live client and an out-pointer live for the call.
        let hr = unsafe { (client.vtbl().get_mix_format)(client.as_raw(), &raw mut raw_mix) };
        check("open", "IAudioClient::GetMixFormat", hr)?;
        let mix = MixFormat(
            NonNull::new(raw_mix).expect("GetMixFormat reported success and returned no format"),
        );
        // SAFETY: `GetMixFormat` succeeded, so `mix` is a `WAVEFORMATEX` followed by its `cbSize`
        // bytes, owned by `mix` and live until it drops at the end of this function.
        let format = parse_mix_format(unsafe { format_bytes(mix.0) }, "open")?;

        let duration = hns_from_frames(buffer_frames, format.rate);
        // SAFETY: a live client; the format is the one `GetMixFormat` just returned, still live;
        // a null session GUID asks for the default session. `hnsPeriodicity` must be zero in
        // shared mode.
        let hr = unsafe {
            (client.vtbl().initialize)(
                client.as_raw(),
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                duration,
                0,
                mix.0.as_ptr(),
                ptr::null(),
            )
        };
        check("open", "IAudioClient::Initialize", hr)?;
        drop(mix);

        let mut buffer = 0u32;
        // SAFETY: a live, initialised client and an out-pointer live for the call.
        let hr = unsafe { (client.vtbl().get_buffer_size)(client.as_raw(), &raw mut buffer) };
        check("open", "IAudioClient::GetBufferSize", hr)?;

        let mut default_period = 0i64;
        // SAFETY: a live client, one out-pointer live for the call, and a null for the minimum
        // period, which the header marks `_Out_opt_`.
        let hr = unsafe {
            (client.vtbl().get_device_period)(
                client.as_raw(),
                &raw mut default_period,
                ptr::null_mut(),
            )
        };
        check("open", "IAudioClient::GetDevicePeriod", hr)?;

        // SAFETY: no security attributes, auto-reset, initially unsignalled, unnamed.
        let raw_event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if raw_event.is_null() {
            return Err(last_error("open", "CreateEventW"));
        }
        // SAFETY: a fresh, valid handle that nothing else owns; `OwnedHandle` closes it once.
        let event = unsafe { OwnedHandle::from_raw_handle(raw_event) };

        // SAFETY: a live, initialised client and a live event handle. The client does not take
        // ownership of the handle; the field order of `AudioOutput` keeps the handle open until
        // after the client is released.
        let hr =
            unsafe { (client.vtbl().set_event_handle)(client.as_raw(), event.as_raw_handle()) };
        check("open", "IAudioClient::SetEventHandle", hr)?;

        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: a live, initialised client, a static IID and an out-pointer live for the call.
        let hr = unsafe {
            (client.vtbl().get_service)(client.as_raw(), &IID_IAudioRenderClient, &raw mut raw)
        };
        // SAFETY: `GetService`, asked for `IID_IAudioRenderClient`, stores an owned
        // `IAudioRenderClient` when it succeeds.
        let render =
            unsafe { take::<IAudioRenderClientVtbl>(hr, raw) }.map_err(|code| AudioError::Os {
                operation: "open",
                api: "IAudioClient::GetService(IAudioRenderClient)",
                code,
            })?;

        Ok(AudioOutput {
            render,
            client,
            _device: device,
            event,
            format,
            buffer_frames: buffer,
            period_frames: frames_from_hns(default_period, format.rate),
            running: false,
            _mta: mta,
        })
        // `enumerator` is released here, then `_thread` balances this thread's `CoInitializeEx`.
    }

    pub(super) fn format(&self) -> OutputFormat {
        self.format.output()
    }

    pub(super) fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    pub(super) fn period_frames(&self) -> u32 {
        self.period_frames
    }

    /// `GetBufferSize` minus `GetCurrentPadding`. `operation` names the seam call that asked, so
    /// that a failure inside `write` or `wait_writable` says so.
    pub(super) fn writable_frames(&self, operation: &'static str) -> AudioResult<u32> {
        let mut padding = 0u32;
        // SAFETY: a live, initialised client and an out-pointer live for the call.
        let hr = unsafe {
            (self.client.vtbl().get_current_padding)(self.client.as_raw(), &raw mut padding)
        };
        check(operation, "IAudioClient::GetCurrentPadding", hr)?;
        // The padding never exceeds the buffer; saturating rather than wrapping all the same,
        // because a wrapped count would be four billion frames of free space (VERIFICATION entry 3).
        Ok(self.buffer_frames.saturating_sub(padding))
    }

    /// `WaitForSingleObject` on the stream's event, then [`AudioOutput::writable_frames`].
    pub(super) fn wait_writable(&self, timeout: Duration) -> AudioResult<u32> {
        // SAFETY: a live event handle owned by `self`, and a finite timeout (`wait_millis` never
        // returns `INFINITE`).
        let waited =
            unsafe { WaitForSingleObject(self.event.as_raw_handle(), wait_millis(timeout)) };
        if waited == WAIT_FAILED {
            return Err(last_error("wait_writable", "WaitForSingleObject"));
        }
        // `WAIT_OBJECT_0` (the engine consumed a period) and `WAIT_TIMEOUT` both come here: either
        // way the answer is what is writable now. `WAIT_ABANDONED` is for mutexes and cannot come
        // from an event.
        self.writable_frames("wait_writable")
    }

    /// `GetBuffer`, copy, `ReleaseBuffer`. The caller has checked that `frames` fits.
    pub(super) fn write(&mut self, samples: &[f32], frames: u32) -> AudioResult<()> {
        let channels = usize::from(self.format.channels.get());
        // What keeps the copy below inside the buffer, checked here rather than trusted from the
        // caller: `GetBuffer` hands over `frames * nBlockAlign` bytes, `parse_mix_format` refused
        // any `nBlockAlign` other than `4 * channels`, and this is the other half.
        assert_eq!(
            samples.len(),
            frames as usize * channels,
            "write: {} samples is not {frames} frames of {channels} channels",
            samples.len()
        );

        let mut data: *mut u8 = ptr::null_mut();
        // SAFETY: a live render client and an out-pointer live for the call.
        let hr =
            unsafe { (self.render.vtbl().get_buffer)(self.render.as_raw(), frames, &raw mut data) };
        check("write", "IAudioRenderClient::GetBuffer", hr)?;
        assert!(
            !data.is_null(),
            "GetBuffer reported success for {frames} frames and returned no buffer"
        );
        // SAFETY: `GetBuffer` succeeded, so `data` is `frames * nBlockAlign` writable bytes that
        // belong to this stream until `ReleaseBuffer`; that is `size_of_val(samples)` bytes by the
        // assertion above. The source is a Rust slice and the destination the engine's buffer, so
        // they cannot overlap; a byte copy needs no alignment.
        unsafe {
            ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), data, size_of_val(samples));
        }
        // SAFETY: releases exactly the frames `GetBuffer` handed over, all of them written, with no
        // flags (`AUDCLNT_BUFFERFLAGS_SILENT` would tell the engine to ignore what was copied).
        let hr = unsafe { (self.render.vtbl().release_buffer)(self.render.as_raw(), frames, 0) };
        check("write", "IAudioRenderClient::ReleaseBuffer", hr)
    }

    pub(super) fn start(&mut self) -> AudioResult<()> {
        if self.running {
            return Ok(());
        }
        // SAFETY: a live, initialised client whose event handle has been set, which `Start`
        // requires of an event-driven stream.
        let hr = unsafe { (self.client.vtbl().start)(self.client.as_raw()) };
        check("start", "IAudioClient::Start", hr)?;
        self.running = true;
        Ok(())
    }

    pub(super) fn stop(&mut self) -> AudioResult<()> {
        if !self.running {
            return Ok(());
        }
        // SAFETY: a live, initialised client.
        let hr = unsafe { (self.client.vtbl().stop)(self.client.as_raw()) };
        check("stop", "IAudioClient::Stop", hr)?;
        self.running = false;
        Ok(())
    }
}

impl Drop for AudioOutput {
    /// Stop a running stream; the fields then release themselves in declaration order.
    fn drop(&mut self) {
        if self.running {
            // SAFETY: a live, initialised client. The result is ignored: `Drop` has nobody to
            // report it to, and the release that follows is unconditional either way.
            unsafe { (self.client.vtbl().stop)(self.client.as_raw()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use core::mem::offset_of;

    use windows_sys::Win32::Media::Audio::{WAVE_FORMAT_PCM, WAVEFORMATEXTENSIBLE_0};
    use windows_sys::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;

    use super::*;

    const SLOT: usize = size_of::<usize>();

    /// Every slot this file calls, at the index the SDK header's C `…Vtbl` struct gives it
    /// (counting `QueryInterface` as 0). A placeholder lost from one of the structs moves every
    /// slot after it and fails here.
    #[test]
    fn vtable_slots_are_where_the_sdk_puts_them() {
        assert_eq!(offset_of!(IUnknownVtbl, release), 2 * SLOT);

        assert_eq!(offset_of!(IMMDeviceEnumeratorVtbl, get_default_audio_endpoint), 4 * SLOT);
        assert_eq!(offset_of!(IMMDeviceVtbl, activate), 3 * SLOT);

        assert_eq!(offset_of!(IAudioClientVtbl, initialize), 3 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, get_buffer_size), 4 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, get_current_padding), 6 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, get_mix_format), 8 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, get_device_period), 9 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, start), 10 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, stop), 11 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, set_event_handle), 13 * SLOT);
        assert_eq!(offset_of!(IAudioClientVtbl, get_service), 14 * SLOT);
        assert_eq!(size_of::<IAudioClientVtbl>(), 15 * SLOT, "GetService is the last slot");

        assert_eq!(offset_of!(IAudioRenderClientVtbl, get_buffer), 3 * SLOT);
        assert_eq!(offset_of!(IAudioRenderClientVtbl, release_buffer), 4 * SLOT);
        assert_eq!(size_of::<IAudioRenderClientVtbl>(), 5 * SLOT);
    }

    /// `E_NOTFOUND` is the value `mmdeviceapi.h` defines, and it — only it — is "no device".
    #[test]
    fn hresult_from_win32_and_the_no_device_code() {
        assert_eq!(E_NOTFOUND as u32, 0x8007_0490);
        assert_eq!(hresult_from_win32(6) as u32, 0x8007_0006, "ERROR_INVALID_HANDLE");
        assert_eq!(hresult_from_win32(0), 0);
        assert_eq!(hresult_from_win32(0x8889_0004), 0x8889_0004_u32 as i32, "already an HRESULT");

        assert_eq!(endpoint_error(E_NOTFOUND), AudioError::NoDevice { operation: "open" });
        let html_help_e_notfound = 0x8000_100D_u32 as HRESULT;
        assert!(
            matches!(endpoint_error(html_help_e_notfound), AudioError::Os { code, .. } if code == html_help_e_notfound),
            "the other E_NOTFOUND is not this one"
        );
    }

    /// The shape Windows' shared-mode mix format has on a stereo device.
    fn float_header(tag: u32, channels: u16, rate: u32, bits: u16, extra: u16) -> WAVEFORMATEX {
        let block = channels * (bits / 8);
        WAVEFORMATEX {
            wFormatTag: tag as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: extra,
        }
    }

    fn extensible(header: WAVEFORMATEX, sub_format: GUID) -> WAVEFORMATEXTENSIBLE {
        WAVEFORMATEXTENSIBLE {
            Format: header,
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: header.wBitsPerSample },
            dwChannelMask: 0x3,
            SubFormat: sub_format,
        }
    }

    /// Parse `format` through the same byte view `open` uses.
    fn parse<T>(format: &T) -> AudioResult<OutputFormat> {
        let format = NonNull::from(format).cast::<WAVEFORMATEX>();
        // SAFETY: `T` is `WAVEFORMATEX` or `WAVEFORMATEXTENSIBLE`, whose `cbSize` the builders
        // above set to at most the bytes that follow the header in `T`.
        parse_mix_format(unsafe { format_bytes(format) }, "open").map(Float32Format::output)
    }

    const EXTENSION: u16 = (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16;

    #[test]
    fn the_sdk_structures_are_the_sizes_the_byte_view_assumes() {
        assert_eq!(size_of::<WAVEFORMATEX>(), 18);
        assert_eq!(size_of::<WAVEFORMATEXTENSIBLE>(), 40);
        assert_eq!(EXTENSION, 22, "the cbSize an extensible format carries");
    }

    #[test]
    fn a_plain_ieee_float_format_is_accepted() {
        let format = float_header(WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 32, 0);
        assert_eq!(parse(&format), Ok(OutputFormat { sample_rate: 48_000, channels: 2 }));
    }

    #[test]
    fn an_extensible_format_with_the_float_subtype_is_accepted() {
        let format = extensible(
            float_header(WAVE_FORMAT_EXTENSIBLE, 6, 44_100, 32, EXTENSION),
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );
        assert_eq!(parse(&format), Ok(OutputFormat { sample_rate: 44_100, channels: 6 }));
    }

    /// The same extensible format with only the `SubFormat` changed to PCM: the GUID is what
    /// decides, not the 32 bits, which a 32-bit integer format also has.
    #[test]
    fn an_extensible_format_with_the_pcm_subtype_is_refused() {
        let format = extensible(
            float_header(WAVE_FORMAT_EXTENSIBLE, 2, 48_000, 32, EXTENSION),
            KSDATAFORMAT_SUBTYPE_PCM,
        );
        assert_eq!(
            parse(&format),
            Err(AudioError::FormatNotFloat { operation: "open", tag: 0xFFFE, bits: 32 })
        );
    }

    #[test]
    fn pcm16_is_refused_naming_its_tag_and_bits() {
        let format = float_header(WAVE_FORMAT_PCM, 2, 44_100, 16, 0);
        let refused = parse(&format).unwrap_err();
        assert_eq!(refused, AudioError::FormatNotFloat { operation: "open", tag: 1, bits: 16 });
        let text = refused.to_string();
        assert!(text.contains("0x0001") && text.contains("16 bits"), "{text}");
    }

    /// An extensible tag whose `cbSize` does not reach the `SubFormat`: the sample type is
    /// unknown, so it is not known to be float — even though the struct behind it says float.
    #[test]
    fn an_extensible_header_too_short_for_its_subformat_is_refused() {
        let mut format = extensible(
            float_header(WAVE_FORMAT_EXTENSIBLE, 2, 48_000, 32, EXTENSION),
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );
        format.Format.cbSize = EXTENSION - 1;
        assert_eq!(
            parse(&format),
            Err(AudioError::FormatNotFloat { operation: "open", tag: 0xFFFE, bits: 32 })
        );
    }

    /// Float in name but not a shape this seam can copy into: 64-bit samples, a block alignment
    /// that disagrees with the channel count, no channels, no rate.
    #[test]
    fn float_formats_whose_frames_do_not_add_up_are_refused() {
        let doubles = float_header(WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 64, 0);
        assert!(parse(&doubles).is_err(), "64-bit float");

        let mut misaligned = float_header(WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 32, 0);
        misaligned.nBlockAlign = 4;
        assert!(parse(&misaligned).is_err(), "a block of one sample for two channels");

        let silent = float_header(WAVE_FORMAT_IEEE_FLOAT, 0, 48_000, 32, 0);
        assert!(parse(&silent).is_err(), "no channels");

        let stopped = float_header(WAVE_FORMAT_IEEE_FLOAT, 2, 0, 32, 0);
        assert!(parse(&stopped).is_err(), "no rate");
    }

    #[test]
    fn frames_and_durations_convert_at_the_rate() {
        let rate = |r| NonZeroU32::new(r).unwrap();
        // The engine's usual 10 ms period, at the two usual rates.
        assert_eq!(hns_from_frames(480, rate(48_000)), 100_000);
        assert_eq!(frames_from_hns(100_000, rate(48_000)), 480);
        assert_eq!(hns_from_frames(441, rate(44_100)), 100_000);
        assert_eq!(frames_from_hns(100_000, rate(44_100)), 441);
        // Rounded up: one frame at 44.1 kHz is 226.76 units, and asking for 226 would be asking
        // for less than a frame.
        assert_eq!(hns_from_frames(1, rate(44_100)), 227);
        assert_eq!(hns_from_frames(0, rate(48_000)), 0);
        // Rounded to nearest on the way back, and total over what `i64` can say.
        assert_eq!(frames_from_hns(226, rate(44_100)), 1);
        assert_eq!(frames_from_hns(-1, rate(48_000)), 0);
        assert_eq!(frames_from_hns(i64::MAX, rate(192_000)), u32::MAX);
        assert_eq!(hns_from_frames(u32::MAX, rate(1)), 42_949_672_950_000_000);
    }

    /// Round-tripping a frame count through a duration gives it back, at every rate a device
    /// plausibly runs at: `hns_from_frames` rounds up by less than one unit, which is far less
    /// than half a frame below 5 MHz, so the nearest-frame rounding lands back on it.
    #[test]
    fn a_frame_count_survives_the_round_trip() {
        for r in [
            8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 192_000, 384_000,
        ] {
            let rate = NonZeroU32::new(r).unwrap();
            for frames in (0..20_000).chain([u32::MAX / 2, u32::MAX]) {
                assert_eq!(
                    frames_from_hns(hns_from_frames(frames, rate), rate),
                    frames,
                    "{frames} at {r}"
                );
            }
        }
    }

    #[test]
    fn timeouts_round_up_to_milliseconds_and_never_become_infinite() {
        assert_eq!(wait_millis(Duration::ZERO), 0);
        assert_eq!(wait_millis(Duration::from_nanos(1)), 1, "a microsecond wait is not a poll");
        assert_eq!(wait_millis(Duration::from_millis(1)), 1);
        assert_eq!(wait_millis(Duration::from_micros(1_500)), 2);
        assert_eq!(wait_millis(Duration::from_millis(u64::from(u32::MAX))), INFINITE - 1);
        assert_eq!(wait_millis(Duration::MAX), INFINITE - 1);
    }
}
