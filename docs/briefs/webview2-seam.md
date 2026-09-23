# Brief: a host web-view seam in `omni-platform` (WebView2)

**DONE 2026-09-23 (`b74f62d`).** Built from this brief, with three additions: `user_agent`,
`WebViewEvent::NonStringMessage` and `WebViewError::InvalidArgument`. Kept for the record.

Written 2026-09-23 for a subagent; the first attempt was stopped when its session ended, before it
had written any code. Relaunch it with this text (adjust the file-ownership list to whoever else is
editing at the time).

## Why

The Roblox app shows web pages in an Android WebView: the captcha "challenge" page at sign-in
(MEASURED: password sign-in answers `"Challenge is required to authorize the request"`, the app
opens `ChallengeHybridWebView`, nothing appears, and 70 s later logs `Load generic challenge
failed`), and sometimes at game join. This runtime has no web view. The Android side
(`com.roblox.protocols.webview.WebViewProtocol`, see HANDOFF's frontier) is emulated in
`crates/omni-android`; it drives this seam. This seam is only the host side: a real browser window
using **Microsoft Edge WebView2**, installed on Windows 11.

## The API (fixed — the Android side codes against it)

```rust
// omni_platform::webview
pub struct WebViewOptions {
    pub title: String,
    pub url: String,
    pub width: u32,            // client size, physical pixels
    pub height: u32,
    /// Added with `AddScriptToExecuteOnDocumentCreated`: runs before any page script, in every
    /// navigation (the top frame at least; document what was done about iframes).
    pub init_script: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum WebViewEvent {
    Ready,                                             // window and browser ready, first navigation issued
    NavigationStarting { url: String },
    NavigationCompleted { url: String, success: bool },
    Message(String),                                   // window.chrome.webview.postMessage(string)
    Closed,                                            // the person closed the window
    Failed(String),                                    // browser process died / environment failed
}
pub type WebViewResult<T> = Result<T, WebViewError>;
// thiserror enum like the crate's other seams: Unsupported { operation, intended, target },
// RuntimeMissing { detail }, Os { operation, api, code: i32 /* HRESULT */ }, Closed { operation }.

/// A top-level host window with a WebView2 filling its client area. `Send`.
pub struct WebView { /* private */ }
impl WebView {
    pub fn runtime_version() -> WebViewResult<String>;          // never opens a window
    pub fn open(options: &WebViewOptions) -> WebViewResult<WebView>; // `Ready` arrives as an event
    pub fn poll_events(&self) -> Vec<WebViewEvent>;             // never blocks
    pub fn execute_script(&self, script: &str) -> WebViewResult<()>;
    pub fn navigate(&self, url: &str) -> WebViewResult<()>;
    pub fn close(&self);                                        // idempotent; Drop closes too
}
```

**Threading (required):** WebView2 needs an STA thread with a message pump. `open` spawns a UI
thread owned by the seam: COM (STA), the window, environment/controller/webview, the pump.
Commands are posted to it (channel + a posted window message to wake the pump); events come back
through a channel read by `poll_events`. The caller never pumps anything — it will be a runtime
thread that does other work.

## How

- **No new crate dependencies if avoidable** (disk is tight; the `windows` crate is large).
  `windows-sys` is already a dependency; add features in `crates/omni-platform/Cargo.toml`,
  commented like the existing ones. WebView2's COM interfaces are not in `windows-sys`: declare the
  vtables you call by hand as `#[repr(C)]` in the correct slot order, verified against `WebView2.h`
  (NuGet `Microsoft.Web.WebView2`; download it to a scratch directory, do not add it to the repo).
  Callback objects (`...CompletedHandler`/`...EventHandler`) need hand-built vtables with
  `QueryInterface`/`AddRef`/`Release`/`Invoke`: reference-counted and freed exactly once.
- **The loader:** `CreateCoreWebView2EnvironmentWithOptions` lives in `WebView2Loader.dll`, which is
  not part of Windows. Either re-implement the loader's job (find the Evergreen runtime through
  `HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`
  and the `HKCU` equivalent, `pv`/`location`; load `EmbeddedBrowserWebView.dll` from
  `<location>\<version>\EBWebView\x64\` and call the export the open-source loaders use — verify the
  export name and signature from a real implementation such as `webview2-com`/`OpenWebView2Loader`,
  not from memory) or justify another option. **Never vendor a DLL into the repo.**
- Registry reads: reuse the `RegGetValueW` pattern in `src/process/windows.rs`.
- User data folder: `%LOCALAPPDATA%\Omnidroid\webview`.
- `unix.rs`: structural backend returning `Unsupported` (as `src/window/`); `cfg(target_os)` only
  inside `omni-platform`.
- Conventions: `#![warn(missing_docs)]`; every `unsafe` block has a `// SAFETY:` comment; say what
  is MEASURED vs assumed; no plausible stubs. **`src/audio/` is the model**: a WASAPI seam with
  hand-written COM vtables, done this session in exactly this style.

## Tests

- Unit tests for pure parts (option/event conversion, registry-path/version parsing, vtable slot
  numbers pinned against the header's order).
- `tests/webview_live.rs`, gated like `tests/audio_live.rs` (`#[ignore = "... OMNI_WEBVIEW_LIVE_TESTS=1
  ..."]` and a `require_gate()` that panics naming the variable under `--ignored` without it):
  `runtime_version()` answers; a `data:text/html,...` page whose script posts `'hello'` → a
  `Message("hello")`; `init_script` runs before the page; `execute_script` of a `postMessage`
  arrives; `navigate` → `NavigationCompleted { success: true }`; `close()` → later commands answer
  `Closed`. Prove at least one assertion can fail (break, run, restore).
- Run `cargo test -p omni-platform --lib`, the live tests with the variable set, and clippy.

## File ownership

`crates/omni-platform/src/webview/**` (new), `crates/omni-platform/tests/webview_live.rs` (new),
`crates/omni-platform/src/lib.rs` (only `pub mod webview;`, added LAST so the crate is never broken),
`crates/omni-platform/Cargo.toml` (only the windows-sys feature list). Nothing else. No commits.
