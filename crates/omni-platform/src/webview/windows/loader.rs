//! Finding the installed WebView2 runtime and calling into it, without `WebView2Loader.dll`.
//!
//! # What `WebView2Loader.dll` does, and why this file does it instead
//!
//! `CreateCoreWebView2EnvironmentWithOptions` is exported by `WebView2Loader.dll`, which ships in
//! the WebView2 SDK and **not** with Windows or with the runtime. Linking it would mean vendoring
//! a DLL, so this file does the loader's job itself. That job is small:
//!
//! 1. Read the string value `EBWebView` of
//!    `Software\Microsoft\EdgeUpdate\ClientState\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` — the
//!    stable (Evergreen) channel's GUID — in the registry's **32-bit view**
//!    (`KEY_WOW64_32KEY`, spelled `RRF_SUBKEY_WOW6432KEY` for `RegGetValueW`), under
//!    `HKEY_CURRENT_USER` first and then `HKEY_LOCAL_MACHINE`. The value is the runtime's
//!    versioned directory, e.g. `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\
//!    153.0.4234.48`, and its last component is the version.
//! 2. Refuse a version older than **86.0.616.0**, the oldest runtime the loader accepts.
//! 3. `LoadLibraryExW` `<that directory>\EBWebView\x64\EmbeddedBrowserWebView.dll`.
//! 4. Call its export `CreateWebViewEnvironmentWithOptionsInternal`, whose signature is
//!    `HRESULT STDMETHODCALLTYPE (bool checkRunningInstance, int runtimeType, PCWSTR userDataDir,
//!    IUnknown* environmentOptions, ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler*)`,
//!    with `true` and runtime type `0` ("installed", as opposed to `1`, a fixed-version runtime
//!    shipped beside the app).
//!
//! # Where each of those was verified
//!
//! The export is **undocumented**, so none of this is from memory or from one source:
//!
//! * **The export name**: MEASURED — the export table of this host's
//!   `EmbeddedBrowserWebView.dll` (153.0.4234.48, x64) lists exactly
//!   `CreateWebViewEnvironmentWithOptionsInternal`, `DllCanUnloadNow` and `GetHandleVerifier`
//!   besides C++ symbols; and Microsoft's own `WebView2Loader.dll` (x64, as shipped in the
//!   `webview2-com-sys` 0.38.2 crate) contains that name as an ASCII string for `GetProcAddress`.
//! * **The signature and the two constant arguments**: two independent open-source loaders agree
//!   — `jchv/OpenWebView2Loader` (`Source/WebView2Loader.cpp`, `CreateWebViewEnvironmentWithClientDll`)
//!   and `webview/webview` (`core/include/webview/detail/platform/windows/webview2/loader.hh`,
//!   `CreateWebViewEnvironmentWithOptionsInternal_t`), both passing `true` and `installed = 0`.
//! * **The registry location**: both loaders read `ClientState\{GUID}\EBWebView` with
//!   `KEY_WOW64_32KEY`; Microsoft's `WebView2Loader.dll` contains the UTF-16 strings
//!   `Software\Microsoft\EdgeUpdate\ClientState\`, `EBWebView` and the GUID. On this host,
//!   MEASURED: `HKLM\SOFTWARE\WOW6432Node\…\ClientState\{F3017226-…}` has `EBWebView =
//!   C:\Program Files (x86)\Microsoft\EdgeWebView\Application\153.0.4234.48`, agreeing with the
//!   `pv` (`153.0.4234.48`) and `location` of the `Clients\{F3017226-…}` key the brief named; the
//!   `HKCU` `ClientState` key exists and has no `EBWebView`, so the `HKLM` fallback is the path
//!   taken here. The order (`HKCU` first) is `OpenWebView2Loader`'s; `webview/webview` reads
//!   `HKLM` first. It matters only on a machine with both a per-user and a per-machine runtime.
//! * **The minimum version** is `OpenWebView2Loader`'s `kMinimumCompatibleVersion`.
//!
//! Not done, deliberately: the beta/dev/canary channels, the `WEBVIEW2_*` environment-variable and
//! policy overrides, and the MSIX-packaged runtime. Each is a branch no machine this project runs
//! on would take, and an untaken branch is not a check (VERIFICATION entry 12). A host without the
//! stable runtime gets [`WebViewError::RuntimeMissing`] naming what was looked for.
//!
//! **The DLL is never unloaded.** The loaders ask `DllCanUnloadNow` and free it; this file keeps
//! it for the life of the process, because freeing a library whose code other WebView2 objects
//! may still be running is not a risk worth one module's worth of address space.

use core::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY, RegGetValueW,
};
use windows_sys::core::HRESULT;

use super::super::{WebViewError, WebViewResult};
use super::{hresult_from_win32, last_error, wide, wide_os};

/// The stable ("Evergreen") WebView2 runtime's EdgeUpdate client GUID.
const STABLE_CHANNEL: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// The key whose `EBWebView` value names the installed runtime's directory.
fn client_state_key() -> String {
    format!("Software\\Microsoft\\EdgeUpdate\\ClientState\\{STABLE_CHANNEL}")
}

/// The oldest runtime `OpenWebView2Loader` accepts (`kMinimumCompatibleVersion`).
const MINIMUM_VERSION: [u32; 4] = [86, 0, 616, 0];

/// The export every loader calls.
const CREATE_EXPORT: &[u8] = b"CreateWebViewEnvironmentWithOptionsInternal\0";

/// `WebView2RunTimeType::kInstalled` in `OpenWebView2Loader`, `webview2_runtime_type::installed`
/// in `webview/webview`.
const RUNTIME_TYPE_INSTALLED: i32 = 0;

/// The export's type: see this module's header for where it was verified.
pub(super) type CreateEnvironment = unsafe extern "system" fn(
    check_running_instance: bool,
    runtime_type: i32,
    user_data_folder: *const u16,
    environment_options: *mut c_void,
    handler: *mut c_void,
) -> HRESULT;

/// The runtime's per-architecture client DLL, relative to its versioned directory.
fn client_dll_relative() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "EBWebView\\arm64\\EmbeddedBrowserWebView.dll"
    } else if cfg!(target_arch = "x86") {
        "EBWebView\\x86\\EmbeddedBrowserWebView.dll"
    } else {
        "EBWebView\\x64\\EmbeddedBrowserWebView.dll"
    }
}

/// An installed runtime that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Runtime {
    /// The version, as the directory names it: `153.0.4234.48`.
    pub(super) version: String,
    /// The client DLL.
    pub(super) dll: PathBuf,
}

/// The four numbers of a dotted version, or `None` unless it is exactly four decimal numbers.
fn parse_version(text: &str) -> Option<[u32; 4]> {
    let mut parts = text.split('.');
    let mut version = [0u32; 4];
    for slot in &mut version {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

/// Judge one `EBWebView` value: the runtime it names, or why it is not usable.
///
/// `exists` is how the DLL's presence is checked — the filesystem in production, a fixed answer in
/// the unit tests, so that the rule and the file are tested apart.
fn judge(directory: &str, exists: impl Fn(&Path) -> bool) -> Result<Runtime, String> {
    let trimmed = directory.trim_end_matches(['\\', '/']);
    let version = trimmed.rsplit(['\\', '/']).next().unwrap_or("");
    let Some(numbers) = parse_version(version) else {
        return Err(format!("`{directory}` does not end in a four-part version"));
    };
    if numbers < MINIMUM_VERSION {
        return Err(format!(
            "version {version} is older than the oldest runtime the WebView2 loader accepts, \
             86.0.616.0"
        ));
    }
    let dll = Path::new(trimmed).join(client_dll_relative());
    if !exists(&dll) {
        return Err(format!("{} does not exist", dll.display()));
    }
    Ok(Runtime { version: version.to_owned(), dll })
}

/// `RegGetValueW` of one `REG_SZ`, in the 32-bit view. `Ok(None)` when the key or the value does
/// not exist; any other failure is the raw Win32 code.
fn read_string(root: HKEY, key: &str, value: &str) -> Result<Option<String>, u32> {
    let key = wide(key);
    let value = wide(value);
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6432KEY;
    let mut bytes: u32 = 0;
    // SAFETY: both names are NUL-terminated UTF-16 that outlive the call; a null buffer with a
    // valid size pointer is the documented size query.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 {
        return Err(status);
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut filled = bytes;
    // SAFETY: `buffer` holds `filled` bytes, the size the query above answered.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            core::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &raw mut filled,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 {
        return Err(status);
    }
    let units = (filled as usize / 2).min(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..units]).trim_end_matches('\0').to_owned()))
}

/// Find the installed stable runtime. See this module's header for the rule.
pub(super) fn locate(operation: &'static str) -> WebViewResult<Runtime> {
    let key = client_state_key();
    let mut tried = Vec::new();
    for (root, name) in [(HKEY_CURRENT_USER, "HKCU"), (HKEY_LOCAL_MACHINE, "HKLM")] {
        match read_string(root, &key, "EBWebView") {
            Ok(None) => tried.push(format!("{name}\\{key} has no EBWebView value")),
            Ok(Some(directory)) => match judge(&directory, Path::exists) {
                Ok(runtime) => return Ok(runtime),
                Err(why) => tried.push(format!("{name}\\{key}\\EBWebView: {why}")),
            },
            Err(code) => {
                return Err(WebViewError::Os {
                    operation,
                    api: "RegGetValueW",
                    code: hresult_from_win32(code),
                });
            }
        }
    }
    Err(WebViewError::RuntimeMissing {
        detail: format!(
            "no usable Microsoft Edge WebView2 (Evergreen) runtime, in the registry's 32-bit view: \
             {}",
            tried.join("; ")
        ),
    })
}

/// `LoadLibraryExW` the runtime's client DLL and find its creation export.
///
/// The module is **never freed** (see this module's header) — except here, when the export is
/// missing, which makes the DLL useless to this seam.
pub(super) fn create_environment_export(runtime: &Runtime) -> WebViewResult<CreateEnvironment> {
    let path = wide_os(runtime.dll.as_os_str());
    // SAFETY: a NUL-terminated UTF-16 path that outlives the call, no reserved handle, and a
    // documented flag: dependencies are searched for beside the DLL rather than beside this
    // process's executable.
    let module: HMODULE =
        unsafe { LoadLibraryExW(path.as_ptr(), core::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
    if module.is_null() {
        return Err(last_error("open", "LoadLibraryExW(EmbeddedBrowserWebView.dll)"));
    }
    // SAFETY: a module handle just returned, and a NUL-terminated ASCII name.
    let export = unsafe { GetProcAddress(module, CREATE_EXPORT.as_ptr()) };
    let Some(export) = export else {
        let error = last_error("open", "GetProcAddress(CreateWebViewEnvironmentWithOptionsInternal)");
        // SAFETY: the handle from the `LoadLibraryExW` above, released once; nothing from the
        // module has been used.
        unsafe { FreeLibrary(module) };
        return Err(error);
    };
    // SAFETY: the export's type is the one this module's header documents and verifies; a
    // function pointer of one `extern "system"` type transmuted to another of the same ABI.
    Ok(unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, CreateEnvironment>(export) })
}

/// Call the creation export: `(true, installed, user data folder, no options, handler)`.
///
/// # Safety
///
/// Must be called on a thread in a single-threaded apartment with a message pump, which WebView2
/// completes the handler through; `user_data_folder` must be NUL-terminated and `handler` a live
/// `ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler`.
pub(super) unsafe fn create_environment(
    create: CreateEnvironment,
    user_data_folder: &[u16],
    handler: *mut c_void,
) -> HRESULT {
    debug_assert_eq!(user_data_folder.last(), Some(&0));
    // SAFETY: the caller's contract; a null options object asks for the defaults, as
    // `CreateCoreWebView2Environment` (no options) does in `OpenWebView2Loader`.
    unsafe {
        create(
            true,
            RUNTIME_TYPE_INSTALLED,
            user_data_folder.as_ptr(),
            core::ptr::null_mut(),
            handler,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_exactly_four_decimal_numbers() {
        assert_eq!(parse_version("153.0.4234.48"), Some([153, 0, 4234, 48]));
        assert_eq!(parse_version("86.0.616.0"), Some([86, 0, 616, 0]));
        for bad in ["", "153.0.4234", "153.0.4234.48.1", "153.0.x.48", "153..4234.48", "+1.0.0.0", " 1.0.0.0"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
    }

    const HERE: &str = r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\153.0.4234.48";

    #[test]
    fn a_directory_names_its_version_and_its_client_dll() {
        let runtime = judge(HERE, |_| true).unwrap();
        assert_eq!(runtime.version, "153.0.4234.48");
        assert_eq!(
            runtime.dll,
            Path::new(HERE).join(r"EBWebView\x64\EmbeddedBrowserWebView.dll"),
            "this is an x86-64 build"
        );
        // A trailing separator names the same runtime.
        assert_eq!(judge(&format!("{HERE}\\"), |_| true), Ok(runtime));
    }

    /// Older than 86.0.616.0 by the last number, by the third, and exactly at it.
    #[test]
    fn the_minimum_version_is_compared_as_numbers() {
        let old = judge(r"C:\x\86.0.615.99", |_| true).unwrap_err();
        assert!(old.contains("older"), "{old}");
        assert!(judge(r"C:\x\85.999.9999.9", |_| true).is_err());
        assert!(judge(r"C:\x\86.0.616.0", |_| true).is_ok(), "the minimum itself is accepted");
        assert!(judge(r"C:\x\100.0.0.0", |_| true).is_ok(), "100 > 86 as numbers, not as text");
    }

    #[test]
    fn a_missing_dll_or_an_unversioned_directory_is_refused_by_name() {
        let missing = judge(HERE, |_| false).unwrap_err();
        assert!(missing.contains("EmbeddedBrowserWebView.dll") && missing.contains("does not exist"), "{missing}");
        let unversioned = judge(r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application", |_| true).unwrap_err();
        assert!(unversioned.contains("four-part version"), "{unversioned}");
    }

    /// The installed runtime this host has, found by the production path, and cross-checked
    /// against the **other** registry record the brief named: `Clients\{GUID}`'s `pv` and
    /// `location`, which this module does not read. Two records that must agree are a check.
    /// Gated because it depends on the host having the runtime.
    #[test]
    #[ignore = "needs the WebView2 runtime: OMNI_WEBVIEW_LIVE_TESTS=1 cargo test -- --ignored"]
    fn the_client_state_record_agrees_with_the_clients_record() {
        assert!(
            std::env::var("OMNI_WEBVIEW_LIVE_TESTS").is_ok_and(|v| v == "1"),
            "run with --ignored but OMNI_WEBVIEW_LIVE_TESTS is not 1"
        );
        let runtime = locate("test").expect("the stable runtime");
        let clients = format!("Software\\Microsoft\\EdgeUpdate\\Clients\\{STABLE_CHANNEL}");
        let pv = read_string(HKEY_LOCAL_MACHINE, &clients, "pv").unwrap().expect("pv");
        let location = read_string(HKEY_LOCAL_MACHINE, &clients, "location").unwrap().expect("location");
        println!("ClientState: {runtime:?}; Clients: pv {pv}, location {location}");
        assert_eq!(runtime.version, pv);
        assert_eq!(runtime.dll, Path::new(&location).join(&pv).join(client_dll_relative()));
        assert!(runtime.dll.exists());
    }
}
