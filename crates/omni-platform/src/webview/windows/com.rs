//! WebView2's COM interfaces, declared by hand, and the callback objects this seam hands it.
//!
//! # Where the slot order comes from
//!
//! `windows-sys` has no WebView2 at all, so every vtable here is a `#[repr(C)]` prefix of the
//! real table, up to the last slot this seam calls, in the style of `audio/windows.rs`. **A wrong
//! slot is a call to a different method**, so the order is not from memory: each struct was
//! transcribed from the C-interface `…Vtbl` structs in `WebView2.h` of the NuGet package
//! `Microsoft.Web.WebView2` **1.0.4191.47**, read from a scratch directory by a script that lists
//! each `STDMETHODCALLTYPE` member in order, and `vtable_slots_are_where_webview2_h_puts_them`
//! pins every called slot to its index there. The interface IDs are that header's
//! `MIDL_INTERFACE` strings.
//!
//! # The callback objects
//!
//! WebView2 reports everything through objects the caller implements: a `…CompletedHandler` for
//! each asynchronous call and an `…EventHandler` for each event. Every one of them is `IUnknown`
//! plus one `Invoke`, and `Invoke` always takes two arguments, which come in exactly two shapes:
//! `(HRESULT, pointer)` for a completion and `(sender, args)` for an event. So there is one object
//! layout, [`Handler`], with one static vtable per shape, and the interface it answers to is a
//! field rather than a type.
//!
//! A handler is reference-counted the COM way: [`HandlerRef`] owns the creator's reference and
//! gives it up when dropped, WebView2 `AddRef`s whatever it keeps, and the last `Release` frees
//! the object and its closure exactly once (`a_handler_is_freed_exactly_once_on_its_last_release`).
//!
//! **One assumption, stated rather than trusted.** WebView2 promises that it *invokes* handlers on
//! the UI thread that created the environment. That it also *releases* them there is ASSUMED. A
//! handler's closure holds a `Weak` to that thread's state, which must not be dropped anywhere
//! else, so a last `Release` arriving on any other thread **leaks** the object instead of freeing
//! it: a bounded leak in place of undefined behaviour.

use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows_sys::Win32::Foundation::{E_FAIL, E_NOINTERFACE, E_POINTER, HWND, RECT, S_OK};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::core::{BOOL, GUID, HRESULT, PWSTR};

// ---------------------------------------------------------------------------------------------
// Interface IDs (`MIDL_INTERFACE` strings in WebView2.h 1.0.4191.47)
// ---------------------------------------------------------------------------------------------

/// `IID_IUnknown`, `00000000-0000-0000-C000-000000000046` (`unknwn.h`).
#[allow(non_upper_case_globals)]
pub(super) const IID_IUnknown: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

/// `ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_EnvironmentCompleted: GUID =
    GUID::from_u128(0x4e8a3389_c9d8_4bd2_b6b5_124fee6cc14d);

/// `ICoreWebView2CreateCoreWebView2ControllerCompletedHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_ControllerCompleted: GUID =
    GUID::from_u128(0x6c4819f3_c9b7_4260_8127_c9f5bde7f68c);

/// `ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_AddScriptCompleted: GUID =
    GUID::from_u128(0xb99369f3_9b11_47b5_bc6f_8e7895fcea17);

/// `ICoreWebView2ExecuteScriptCompletedHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_ExecuteScriptCompleted: GUID =
    GUID::from_u128(0x49511172_cc67_4bca_9923_137112f4c4cc);

/// `ICoreWebView2NavigationStartingEventHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_NavigationStartingHandler: GUID =
    GUID::from_u128(0x9adbe429_f36d_432b_9ddc_f8881fbd76e3);

/// `ICoreWebView2NavigationCompletedEventHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_NavigationCompletedHandler: GUID =
    GUID::from_u128(0xd33a35bf_1c49_4f98_93ab_006e0533fe1c);

/// `ICoreWebView2WebMessageReceivedEventHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_WebMessageReceivedHandler: GUID =
    GUID::from_u128(0x57213f19_00e6_49fa_8e07_898ea01ecbd2);

/// `ICoreWebView2ProcessFailedEventHandler`.
#[allow(non_upper_case_globals)]
pub(super) const IID_ProcessFailedHandler: GUID =
    GUID::from_u128(0x79e0aea4_990b_42d9_aa1d_0fcc2e5bc7f1);

/// `ICoreWebView2Settings2`, the settings interface with `UserAgent`.
#[allow(non_upper_case_globals)]
pub(super) const IID_Settings2: GUID = GUID::from_u128(0xee9a0f68_f46c_4e32_ac23_ef8cac224d2a);

/// `COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC`, `= 0` in WebView2.h.
pub(super) const MOVE_FOCUS_REASON_PROGRAMMATIC: i32 = 0;

/// `EventRegistrationToken` (`eventtoken.h`): one `__int64`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct EventRegistrationToken {
    pub(super) value: i64,
}

// ---------------------------------------------------------------------------------------------
// Vtables (C `…Vtbl` structs in WebView2.h 1.0.4191.47, as prefixes)
// ---------------------------------------------------------------------------------------------

/// A slot this seam never calls, present so the slots after it sit where the header puts them.
type Unused = *const c_void;

/// `add_…` event registration: `(This, handler, token*)`.
pub(super) type AddHandler = unsafe extern "system" fn(
    this: *mut c_void,
    handler: *mut c_void,
    token: *mut EventRegistrationToken,
) -> HRESULT;

/// `remove_…` event registration: `(This, token)`.
pub(super) type RemoveHandler =
    unsafe extern "system" fn(this: *mut c_void, token: EventRegistrationToken) -> HRESULT;

/// A property getter handing back a `CoTaskMemAlloc`'d string: `(This, LPWSTR*)`.
pub(super) type GetString = unsafe extern "system" fn(this: *mut c_void, value: *mut PWSTR) -> HRESULT;

/// `IUnknownVtbl`: the three slots every COM vtable begins with.
#[repr(C)]
pub(super) struct IUnknownVtbl {
    pub(super) query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> HRESULT,
    pub(super) add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub(super) release: unsafe extern "system" fn(this: *mut c_void) -> u32,
}

/// `ICoreWebView2EnvironmentVtbl`, up to `CreateCoreWebView2Controller`.
#[repr(C)]
pub(super) struct EnvironmentVtbl {
    _base: IUnknownVtbl,
    pub(super) create_controller: unsafe extern "system" fn(
        this: *mut c_void,
        parent: HWND,
        handler: *mut c_void,
    ) -> HRESULT,
}

/// `ICoreWebView2ControllerVtbl`, whole: `get_CoreWebView2` is its last slot.
#[repr(C)]
pub(super) struct ControllerVtbl {
    _base: IUnknownVtbl,
    _get_is_visible: Unused,
    pub(super) put_is_visible: unsafe extern "system" fn(this: *mut c_void, visible: BOOL) -> HRESULT,
    _get_bounds: Unused,
    pub(super) put_bounds: unsafe extern "system" fn(this: *mut c_void, bounds: RECT) -> HRESULT,
    _get_zoom_factor: Unused,
    _put_zoom_factor: Unused,
    _add_zoom_factor_changed: Unused,
    _remove_zoom_factor_changed: Unused,
    _set_bounds_and_zoom_factor: Unused,
    pub(super) move_focus: unsafe extern "system" fn(this: *mut c_void, reason: i32) -> HRESULT,
    _add_move_focus_requested: Unused,
    _remove_move_focus_requested: Unused,
    _add_got_focus: Unused,
    _remove_got_focus: Unused,
    _add_lost_focus: Unused,
    _remove_lost_focus: Unused,
    _add_accelerator_key_pressed: Unused,
    _remove_accelerator_key_pressed: Unused,
    _get_parent_window: Unused,
    _put_parent_window: Unused,
    _notify_parent_window_position_changed: Unused,
    pub(super) close: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    pub(super) get_core_webview2:
        unsafe extern "system" fn(this: *mut c_void, webview: *mut *mut c_void) -> HRESULT,
}

/// `ICoreWebView2Vtbl`, up to `remove_WebMessageReceived` (slot 35 of 61).
#[repr(C)]
pub(super) struct CoreWebView2Vtbl {
    _base: IUnknownVtbl,
    pub(super) get_settings:
        unsafe extern "system" fn(this: *mut c_void, settings: *mut *mut c_void) -> HRESULT,
    pub(super) get_source: GetString,
    pub(super) navigate: unsafe extern "system" fn(this: *mut c_void, uri: *const u16) -> HRESULT,
    _navigate_to_string: Unused,
    pub(super) add_navigation_starting: AddHandler,
    pub(super) remove_navigation_starting: RemoveHandler,
    _add_content_loading: Unused,
    _remove_content_loading: Unused,
    _add_source_changed: Unused,
    _remove_source_changed: Unused,
    _add_history_changed: Unused,
    _remove_history_changed: Unused,
    pub(super) add_navigation_completed: AddHandler,
    pub(super) remove_navigation_completed: RemoveHandler,
    _add_frame_navigation_starting: Unused,
    _remove_frame_navigation_starting: Unused,
    _add_frame_navigation_completed: Unused,
    _remove_frame_navigation_completed: Unused,
    _add_script_dialog_opening: Unused,
    _remove_script_dialog_opening: Unused,
    _add_permission_requested: Unused,
    _remove_permission_requested: Unused,
    pub(super) add_process_failed: AddHandler,
    pub(super) remove_process_failed: RemoveHandler,
    pub(super) add_script_to_execute_on_document_created: unsafe extern "system" fn(
        this: *mut c_void,
        script: *const u16,
        handler: *mut c_void,
    ) -> HRESULT,
    _remove_script_to_execute_on_document_created: Unused,
    pub(super) execute_script: unsafe extern "system" fn(
        this: *mut c_void,
        script: *const u16,
        handler: *mut c_void,
    ) -> HRESULT,
    _capture_preview: Unused,
    _reload: Unused,
    _post_web_message_as_json: Unused,
    _post_web_message_as_string: Unused,
    pub(super) add_web_message_received: AddHandler,
    pub(super) remove_web_message_received: RemoveHandler,
}

/// `ICoreWebView2Settings2Vtbl`, whole: `ICoreWebView2Settings`' eighteen property slots, then
/// `get_UserAgent` and `put_UserAgent`.
#[repr(C)]
pub(super) struct Settings2Vtbl {
    _base: IUnknownVtbl,
    _settings: [Unused; 18],
    _get_user_agent: Unused,
    pub(super) put_user_agent:
        unsafe extern "system" fn(this: *mut c_void, value: *const u16) -> HRESULT,
}

/// `ICoreWebView2NavigationStartingEventArgsVtbl`, whole: `get_NavigationId` is its last slot.
#[repr(C)]
pub(super) struct NavigationStartingArgsVtbl {
    _base: IUnknownVtbl,
    pub(super) get_uri: GetString,
    _get_is_user_initiated: Unused,
    _get_is_redirected: Unused,
    _get_request_headers: Unused,
    _get_cancel: Unused,
    _put_cancel: Unused,
    pub(super) get_navigation_id: unsafe extern "system" fn(this: *mut c_void, id: *mut u64) -> HRESULT,
}

/// `ICoreWebView2NavigationCompletedEventArgsVtbl`, whole.
#[repr(C)]
pub(super) struct NavigationCompletedArgsVtbl {
    _base: IUnknownVtbl,
    pub(super) get_is_success: unsafe extern "system" fn(this: *mut c_void, success: *mut BOOL) -> HRESULT,
    _get_web_error_status: Unused,
    pub(super) get_navigation_id: unsafe extern "system" fn(this: *mut c_void, id: *mut u64) -> HRESULT,
}

/// `ICoreWebView2WebMessageReceivedEventArgsVtbl`, whole.
#[repr(C)]
pub(super) struct WebMessageArgsVtbl {
    _base: IUnknownVtbl,
    _get_source: Unused,
    pub(super) get_web_message_as_json: GetString,
    pub(super) try_get_web_message_as_string: GetString,
}

/// `ICoreWebView2ProcessFailedEventArgsVtbl`, whole.
#[repr(C)]
pub(super) struct ProcessFailedArgsVtbl {
    _base: IUnknownVtbl,
    pub(super) get_process_failed_kind:
        unsafe extern "system" fn(this: *mut c_void, kind: *mut i32) -> HRESULT,
}

// ---------------------------------------------------------------------------------------------
// Owned interface pointers
// ---------------------------------------------------------------------------------------------

/// An owned reference to a COM interface whose vtable begins with `V`'s slots.
///
/// The same type as `audio/windows.rs`'s, plus `Clone` (an `AddRef`): the UI thread clones an
/// interface out of its state before calling it, so that no `RefCell` borrow is held across a
/// call that might re-enter the window procedure.
pub(super) struct Com<V> {
    this: NonNull<*const V>,
}

impl<V> Com<V> {
    /// Take ownership of one reference, or `None` for null.
    ///
    /// # Safety
    ///
    /// `raw` must be null, or an interface pointer whose vtable begins with `V`'s slots and of
    /// which the caller owns one reference. That reference passes to the returned value.
    pub(super) unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw.cast::<*const V>()).map(|this| Com { this })
    }

    /// `AddRef` an interface pointer the caller was only lent — an `Invoke` argument — and own
    /// that new reference. `None` for null.
    ///
    /// # Safety
    ///
    /// `raw` must be null, or a live interface pointer whose vtable begins with `V`'s slots.
    pub(super) unsafe fn from_borrowed(raw: *mut c_void) -> Option<Self> {
        let this = NonNull::new(raw.cast::<*const V>())?;
        // SAFETY: a live COM object (this function's contract); every vtable begins with
        // `IUnknown`'s slots, and `AddRef` takes the reference the returned value will own.
        unsafe {
            let unknown = *this.as_ptr().cast::<*const IUnknownVtbl>();
            ((*unknown).add_ref)(raw);
        }
        Some(Com { this })
    }

    /// The interface pointer, for passing as a method's `This`.
    pub(super) fn as_raw(&self) -> *mut c_void {
        self.this.as_ptr().cast()
    }

    /// The vtable.
    pub(super) fn vtbl(&self) -> &V {
        // SAFETY: by `from_raw`'s contract `this` is a live COM object whose first word points at
        // a vtable beginning with `V`, and it stays live while `self` holds its reference.
        unsafe { &**self.this.as_ptr() }
    }

    /// `QueryInterface` for `iid`, whose vtable must begin with `W`'s slots; the failing `HRESULT`
    /// (`E_NOINTERFACE` for an interface the object lacks) otherwise.
    pub(super) fn query<W>(&self, iid: &GUID) -> Result<Com<W>, HRESULT> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live object, a GUID and an out-pointer live for the call.
        let hr = unsafe { (self.unknown().query_interface)(self.as_raw(), iid, &raw mut raw) };
        if hr < 0 {
            return Err(hr);
        }
        // SAFETY: a successful `QueryInterface` hands over one reference to the interface `iid`
        // names, whose slots the caller vouches `W` begins with.
        unsafe { Com::from_raw(raw) }.ok_or(E_POINTER)
    }

    fn unknown(&self) -> &IUnknownVtbl {
        // SAFETY: every COM vtable begins with `IUnknown`'s three slots, whatever `V` is, and the
        // object is live while `self` holds its reference.
        unsafe { &**self.this.as_ptr().cast::<*const IUnknownVtbl>() }
    }
}

impl<V> Clone for Com<V> {
    fn clone(&self) -> Self {
        // SAFETY: a live object (this value holds a reference); the new reference is owned by the
        // returned value.
        unsafe { (self.unknown().add_ref)(self.as_raw()) };
        Com { this: self.this }
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one reference and gives it up exactly once, here. The
        // returned count is advisory and is ignored, as COM says to.
        unsafe { (self.unknown().release)(self.as_raw()) };
    }
}

/// Call a getter that hands back a `CoTaskMemAlloc`'d UTF-16 string, copy it, and free it.
///
/// A null string from a successful call is the empty string. A failing call is its `HRESULT`.
///
/// # Safety
///
/// `this` must be a live interface pointer whose vtable holds `get` at the slot it was read from.
pub(super) unsafe fn co_string(this: *mut c_void, get: GetString) -> Result<String, HRESULT> {
    let mut raw: PWSTR = core::ptr::null_mut();
    // SAFETY: a live object (this function's contract) and an out-pointer live for the call.
    let hr = unsafe { get(this, &raw mut raw) };
    if hr < 0 {
        return Err(hr);
    }
    if raw.is_null() {
        return Ok(String::new());
    }
    // SAFETY: a successful getter returns a NUL-terminated string it allocated for the caller.
    let text = unsafe {
        let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
        String::from_utf16_lossy(core::slice::from_raw_parts(raw, len))
    };
    // SAFETY: the string was allocated with `CoTaskMemAlloc` (the WebView2 string convention the
    // header states on every such getter) and ownership passed to this function; freed once.
    unsafe { CoTaskMemFree(raw.cast_const().cast()) };
    Ok(text)
}

// ---------------------------------------------------------------------------------------------
// Callback objects
// ---------------------------------------------------------------------------------------------

/// A completion callback: `Invoke(HRESULT errorCode, <pointer> result)`. The pointer is an
/// interface (environment, controller) or an `LPCWSTR` (a script id, a script's JSON result),
/// lent for the length of the call.
pub(super) type Completed = Box<dyn Fn(HRESULT, *mut c_void)>;

/// An event callback: `Invoke(sender, args)`. Only the args are passed on; every event this seam
/// registers is on the one `ICoreWebView2` it already holds. The args are lent for the call.
pub(super) type Event = Box<dyn Fn(*mut c_void)>;

enum Callback {
    Completed(Completed),
    Event(Event),
}

/// The vtable of a completion handler.
#[repr(C)]
struct CompletedVtbl {
    base: IUnknownVtbl,
    invoke: unsafe extern "system" fn(this: *mut c_void, code: HRESULT, result: *mut c_void) -> HRESULT,
}

/// The vtable of an event handler.
#[repr(C)]
struct EventVtbl {
    base: IUnknownVtbl,
    invoke: unsafe extern "system" fn(this: *mut c_void, sender: *mut c_void, args: *mut c_void) -> HRESULT,
}

static COMPLETED_VTBL: CompletedVtbl = CompletedVtbl {
    base: IUnknownVtbl { query_interface, add_ref, release },
    invoke: invoke_completed,
};

static EVENT_VTBL: EventVtbl = EventVtbl {
    base: IUnknownVtbl { query_interface, add_ref, release },
    invoke: invoke_event,
};

/// One callback object. `vtbl` is first, so a pointer to this struct is a COM interface pointer.
#[repr(C)]
struct Handler {
    vtbl: *const c_void,
    refs: AtomicU32,
    /// The one interface besides `IUnknown` that `QueryInterface` answers to.
    iid: GUID,
    /// The thread that created it, which is the only one allowed to free it. See this module's
    /// "One assumption".
    owner: u32,
    callback: Callback,
}

/// The creator's reference to a [`Handler`]: pass [`HandlerRef::as_raw`] to WebView2, which
/// `AddRef`s it if it keeps it, and let this drop.
pub(super) struct HandlerRef(Com<IUnknownVtbl>);

impl HandlerRef {
    /// A completion handler answering to `iid`.
    pub(super) fn completed(iid: GUID, callback: impl Fn(HRESULT, *mut c_void) + 'static) -> Self {
        Self::new(iid, (&raw const COMPLETED_VTBL).cast(), Callback::Completed(Box::new(callback)))
    }

    /// An event handler answering to `iid`.
    pub(super) fn event(iid: GUID, callback: impl Fn(*mut c_void) + 'static) -> Self {
        Self::new(iid, (&raw const EVENT_VTBL).cast(), Callback::Event(Box::new(callback)))
    }

    fn new(iid: GUID, vtbl: *const c_void, callback: Callback) -> Self {
        // SAFETY: no arguments; reads the calling thread's id.
        let owner = unsafe { GetCurrentThreadId() };
        let raw = Box::into_raw(Box::new(Handler {
            vtbl,
            refs: AtomicU32::new(1),
            iid,
            owner,
            callback,
        }));
        // SAFETY: `raw` is a fresh `Handler`, whose first word points at a vtable beginning with
        // `IUnknown`'s slots, and the one reference its count starts at passes to this value.
        HandlerRef(unsafe { Com::from_raw(raw.cast()) }.expect("Box::into_raw is never null"))
    }

    /// The interface pointer to hand to WebView2.
    pub(super) fn as_raw(&self) -> *mut c_void {
        self.0.as_raw()
    }
}

/// GUID equality; `windows-sys`'s `GUID` does not implement `PartialEq`.
pub(super) fn same_guid(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

unsafe extern "system" fn query_interface(
    this: *mut c_void,
    iid: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT {
    if object.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM calls this with `this` one of this module's `Handler`s and `iid` a readable
    // GUID (or null, refused below).
    let handler = unsafe { &*this.cast::<Handler>() };
    // SAFETY: `iid` is a readable GUID when non-null.
    let known = !iid.is_null() && unsafe {
        same_guid(&*iid, &IID_IUnknown) || same_guid(&*iid, &handler.iid)
    };
    if !known {
        // SAFETY: `object` is a writable out-pointer (checked non-null above); COM requires it to
        // be nulled on failure.
        unsafe { *object = core::ptr::null_mut() };
        return E_NOINTERFACE;
    }
    handler.refs.fetch_add(1, Ordering::Relaxed);
    // SAFETY: as above; the reference just taken passes to the caller.
    unsafe { *object = this };
    S_OK
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: COM calls this with a live `Handler` of this module's.
    let handler = unsafe { &*this.cast::<Handler>() };
    handler.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: COM calls this with a live `Handler` of this module's, holding a reference.
    let handler = unsafe { &*this.cast::<Handler>() };
    let left = handler.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        fence(Ordering::Acquire);
        // SAFETY: no arguments; reads the calling thread's id.
        if unsafe { GetCurrentThreadId() } == handler.owner {
            // SAFETY: the count reached zero, so nobody holds a reference and nothing can reach
            // the object again; it came from `Box::into_raw` in `HandlerRef::new`.
            drop(unsafe { Box::from_raw(this.cast::<Handler>()) });
        }
        // Otherwise leaked on purpose; see this module's "One assumption".
    }
    left
}

/// Run a callback, turning a panic into `E_FAIL` rather than letting it unwind into WebView2:
/// unwinding out of an `extern "system"` function aborts the process, and a bug in one web view's
/// event handling must not take the runtime with it. The panic's message is still printed by the
/// panic hook.
pub(super) fn guarded(call: impl FnOnce()) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn invoke_completed(
    this: *mut c_void,
    code: HRESULT,
    result: *mut c_void,
) -> HRESULT {
    // SAFETY: COM calls this through `COMPLETED_VTBL` with a live `Handler` built with a
    // `Callback::Completed`.
    let handler = unsafe { &*this.cast::<Handler>() };
    match &handler.callback {
        Callback::Completed(callback) => guarded(|| callback(code, result)),
        Callback::Event(_) => E_FAIL,
    }
}

unsafe extern "system" fn invoke_event(
    this: *mut c_void,
    _sender: *mut c_void,
    args: *mut c_void,
) -> HRESULT {
    // SAFETY: COM calls this through `EVENT_VTBL` with a live `Handler` built with a
    // `Callback::Event`.
    let handler = unsafe { &*this.cast::<Handler>() };
    match &handler.callback {
        Callback::Event(callback) => guarded(|| callback(args)),
        Callback::Completed(_) => E_FAIL,
    }
}

#[cfg(test)]
mod tests {
    use core::mem::offset_of;
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    const SLOT: usize = size_of::<usize>();

    /// Every slot this seam calls, at the index WebView2.h 1.0.4191.47's C `…Vtbl` struct gives it
    /// (counting `QueryInterface` as 0). A placeholder lost from a struct moves every slot after
    /// it and fails here.
    #[test]
    fn vtable_slots_are_where_webview2_h_puts_them() {
        assert_eq!(offset_of!(IUnknownVtbl, release), 2 * SLOT);

        assert_eq!(offset_of!(EnvironmentVtbl, create_controller), 3 * SLOT);

        assert_eq!(offset_of!(ControllerVtbl, put_is_visible), 4 * SLOT);
        assert_eq!(offset_of!(ControllerVtbl, put_bounds), 6 * SLOT);
        assert_eq!(offset_of!(ControllerVtbl, move_focus), 12 * SLOT);
        assert_eq!(offset_of!(ControllerVtbl, close), 24 * SLOT);
        assert_eq!(offset_of!(ControllerVtbl, get_core_webview2), 25 * SLOT);
        assert_eq!(size_of::<ControllerVtbl>(), 26 * SLOT, "get_CoreWebView2 is the last slot");

        assert_eq!(offset_of!(CoreWebView2Vtbl, get_settings), 3 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, get_source), 4 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, navigate), 5 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, add_navigation_starting), 7 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, remove_navigation_starting), 8 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, add_navigation_completed), 15 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, remove_navigation_completed), 16 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, add_process_failed), 25 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, remove_process_failed), 26 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, add_script_to_execute_on_document_created), 27 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, execute_script), 29 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, add_web_message_received), 34 * SLOT);
        assert_eq!(offset_of!(CoreWebView2Vtbl, remove_web_message_received), 35 * SLOT);

        assert_eq!(offset_of!(Settings2Vtbl, put_user_agent), 22 * SLOT);
        assert_eq!(size_of::<Settings2Vtbl>(), 23 * SLOT, "put_UserAgent is the last slot");

        assert_eq!(offset_of!(NavigationStartingArgsVtbl, get_uri), 3 * SLOT);
        assert_eq!(offset_of!(NavigationStartingArgsVtbl, get_navigation_id), 9 * SLOT);
        assert_eq!(size_of::<NavigationStartingArgsVtbl>(), 10 * SLOT);

        assert_eq!(offset_of!(NavigationCompletedArgsVtbl, get_is_success), 3 * SLOT);
        assert_eq!(offset_of!(NavigationCompletedArgsVtbl, get_navigation_id), 5 * SLOT);
        assert_eq!(size_of::<NavigationCompletedArgsVtbl>(), 6 * SLOT);

        assert_eq!(offset_of!(WebMessageArgsVtbl, get_web_message_as_json), 4 * SLOT);
        assert_eq!(offset_of!(WebMessageArgsVtbl, try_get_web_message_as_string), 5 * SLOT);
        assert_eq!(size_of::<WebMessageArgsVtbl>(), 6 * SLOT);

        assert_eq!(offset_of!(ProcessFailedArgsVtbl, get_process_failed_kind), 3 * SLOT);

        assert_eq!(offset_of!(CompletedVtbl, invoke), 3 * SLOT, "Invoke follows IUnknown");
        assert_eq!(offset_of!(EventVtbl, invoke), 3 * SLOT, "Invoke follows IUnknown");
        assert_eq!(size_of::<EventRegistrationToken>(), 8);
    }

    /// The `IUnknownVtbl` of a raw handler, as WebView2 would reach it.
    fn unknown(raw: *mut c_void) -> &'static IUnknownVtbl {
        // SAFETY: `raw` is a live handler whose first word points at a static vtable.
        unsafe { &**raw.cast::<*const IUnknownVtbl>() }
    }

    /// Driven through the vtable exactly as WebView2 drives it: `QueryInterface` for the handler's
    /// own IID and for `IUnknown` takes a reference, for anything else refuses and nulls the
    /// out-pointer, and the object — with its closure — is freed on the **last** `Release` and not
    /// before.
    #[test]
    fn a_handler_is_freed_exactly_once_on_its_last_release() {
        struct Dropped(Rc<Cell<u32>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        let invoked = Rc::new(Cell::new(None));
        let guard = Dropped(drops.clone());
        let seen = invoked.clone();
        let handler = HandlerRef::completed(IID_ExecuteScriptCompleted, move |code, result| {
            let _ = &guard;
            seen.set(Some((code, result as usize)));
        });
        let raw = handler.as_raw();
        let vtbl = unknown(raw);

        let mut out: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live handler, a static IID and a writable out-pointer.
        assert_eq!(unsafe { (vtbl.query_interface)(raw, &IID_ExecuteScriptCompleted, &raw mut out) }, S_OK);
        assert_eq!(out, raw);
        // SAFETY: as above.
        assert_eq!(unsafe { (vtbl.query_interface)(raw, &IID_IUnknown, &raw mut out) }, S_OK);
        out = raw;
        // SAFETY: as above.
        let refused = unsafe { (vtbl.query_interface)(raw, &IID_NavigationStartingHandler, &raw mut out) };
        assert_eq!(refused, E_NOINTERFACE);
        assert!(out.is_null(), "a refused QueryInterface must null its out-pointer");

        // Invoke through the completion vtable's fourth slot.
        // SAFETY: a completion handler's vtable is a `CompletedVtbl`.
        let invoke = unsafe { (*raw.cast::<*const CompletedVtbl>()).as_ref().unwrap().invoke };
        // SAFETY: a live handler; the result pointer is never dereferenced by this closure.
        assert_eq!(unsafe { invoke(raw, -5, 0x1234 as *mut c_void) }, S_OK);
        assert_eq!(invoked.get(), Some((-5, 0x1234)));

        // Three references now: the creator's and the two `QueryInterface` took.
        // SAFETY: a live handler; each call gives back one reference this test took.
        assert_eq!(unsafe { (vtbl.release)(raw) }, 2);
        // SAFETY: as above.
        assert_eq!(unsafe { (vtbl.release)(raw) }, 1);
        assert_eq!(drops.get(), 0, "freed while a reference was still held");
        drop(handler);
        assert_eq!(drops.get(), 1, "the last Release must free the closure, once");
    }

    /// A panic inside a callback comes back as `E_FAIL` instead of unwinding into the caller.
    #[test]
    fn a_panicking_callback_answers_e_fail() {
        let handler = HandlerRef::event(IID_WebMessageReceivedHandler, |_| panic!("deliberate"));
        let raw = handler.as_raw();
        // SAFETY: an event handler's vtable is an `EventVtbl`.
        let invoke = unsafe { (*raw.cast::<*const EventVtbl>()).as_ref().unwrap().invoke };
        // SAFETY: a live handler; the closure never reads its arguments.
        let hr = unsafe { invoke(raw, core::ptr::null_mut(), core::ptr::null_mut()) };
        assert_eq!(hr, E_FAIL);
    }
}
