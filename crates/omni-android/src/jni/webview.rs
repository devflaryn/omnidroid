//! **The web view**: the Java side's `WebViewProtocol`, the Roblox web view it opens (`ri.a`) and
//! the page's bridge object, run by the host over a host browser window -- so that the pages the
//! engine opens, the sign-in captcha first, are shown to the person, and what the page answers
//! reaches the engine.
//!
//! # The path a device takes, DECODED from `classes2.dex` and `libroblox.so`
//!
//! * **Construction, at startup.** `MainGameActivity.B2` ("Set up context and start to unpack
//!   assets") posts `jk.c1` to the UI thread, which forces `MainGameActivity.l2()`: the lazy
//!   `fh.c` ("rbx.browserservice"), whose factory forces `k2()`, the lazy `jk.a0` -- and `jk.a0`'s
//!   constructor builds `new WebViewProtocol(this)`. That constructor ([`WebViewProtocol::install`]):
//!   `MessageBus.p(protocol, isAvailableId, WebViewProtocol$a)` -- `setRequestHandlerRaw` with a
//!   `MessageBus$b` wrapping the handler -- then three `MessageBus.t(getMessageId(protocol, id),
//!   callback)`, `doSubscribeRaw` with a `MessageBus$a` each (`openWindow`, `mutateWindow`,
//!   `closeWindow`), then the static native `initializeAndroidWebViewProtocol`, which installs the
//!   engine's `AndroidWebViewProtocol` platform object (`0x2bac4bc`: a 0x20-byte object, vtable
//!   `0x63af6c0`, stored in a singleton's slot when it is empty). Then `fh.c.c()` binds three
//!   `MemStorage` keys: `BrowserService.OpenBrowserWindow`, `.CloseBrowserWindow`, `.SendCommand`.
//! * **`isAvailable`** (`WebViewProtocol$a`): `{<getAvailableKey()>: true}`, whatever was asked.
//! * **`openWindow`** (`WebViewProtocol$b`): the URL (`getUrlKey`; none logs "Attempted to open
//!   WebView window with no URL" and stops), the title, the visibility (default `true`), and the
//!   header options, into a `vl.a`; `jk.a0.g` puts a `WebDialogFragment_BrowserService` fragment
//!   up, whose view (`com.roblox.client.c.J0`) is `ri.a` -- an `RBHybridWebView` -- with the app's
//!   user agent ([`user_agent`]), JavaScript and DOM storage on, the URL loaded, and then `ri.a.h()`
//!   binds `MemStorage` `BrowserService.ExecuteJavaScript` for the engine to run script in the page.
//! * **`mutateWindow`** (`WebViewProtocol$c`, `com.roblox.client.c.H3`): stores the visibility
//!   (`D3`) and nothing else; the URL and title it carries are not used.
//! * **`closeWindow`** (`WebViewProtocol$d`, `jk.a0.d`): pops the fragment and, when there was one,
//!   publishes `handleWindowClose` (`WebViewProtocol.v`: `publishRaw(id, "{}")`). Its view's
//!   `onDestroyView` disconnects the `ExecuteJavaScript` binding (`ri.a.i`).
//! * **The page's way back.** `cl.d.d` adds the bridge object `cl.d$c` as
//!   **`__globalRobloxAndroidBridge__`**, one method: **`executeRoblox(String)`**. In `ri.a` the
//!   handler (`ri.a.e`) hands the raw string to the fragment's listener -- `c$a` -> `jk.y` ->
//!   `jk.a0.f` -> `WebViewProtocol.u` -> the static native **`signalJavascriptCallback`**. The
//!   engine (`0x2bac648`) wraps it as `{"command": <string>}` and publishes it on its message bus
//!   as `WebView`/`handleJavascriptCallback` when either of its flags `EnableWebViewService`
//!   (`0x6c6f970`) or `EnableAndroidWebViewService` (`0x6c6f988`) is set -- and its startup sets
//!   both, unconditionally (`0x2bd58a0`..`0x2bd58f0`).
//! * **The person going back** (`jk.a0.h`, the fragment's back callback): with no page to go back
//!   to, publishes `handleWindowClose`; the fragment stays until the engine closes it.
//!
//! # One Java flag, stated
//!
//! The listener that carries the page's string to `signalJavascriptCallback` is attached only when
//! the Java flag `EnableAndroidWebViewService4` (`di.a.K4`, `ci.i.V0()`) is on; off, `ri.a.e`
//! fires `MemStorage` `BrowserService.JavaScriptCallback` instead, and the fragment is built
//! without `ENABLE_WEB_VIEW_SERVICE`. Its compiled default is `false`; a device reads the value
//! Roblox's settings service sends (`FlagCache`), which this host does not fetch. **This host
//! runs the flag-on path**, because the engine's side of it is the one the engine forces on (its
//! own `EnableWebViewService`/`EnableAndroidWebViewService`, above). If a run shows the engine
//! waiting on `BrowserService.JavaScriptCallback` instead, this is the assumption that is wrong.
//!
//! # Not modelled, said so
//!
//! * `MessageBus$b.run` reports telemetry (`reportProtocolMethodResponseTelemetryData`, an
//!   instance native) before it returns. That call would have to be made from inside the engine's
//!   own call into the handler; it is not made. Nothing but Roblox's telemetry reads it.
//! * `BrowserService.OpenBrowserWindow` builds a one-argument `vl.a`, whose visibility is `null`,
//!   and `jk.a0$b.a` throws `NullPointerException` on it (`checkNotNullExpressionValue(visible)`)
//!   -- this app version cannot open a window that way. Refused here, naming that.
//! * `BrowserService.SendCommand`'s commands are not decoded. Refused, naming the command.
//! * The fragment's header, back navigation within the page, deep links out of it (`c$k.g`), and
//!   the web view's cookie sync. A page that needs one says so in the log.

use std::sync::Arc;

use omni_cpu::GuestCpu;
use omni_mem::GuestAddr;

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::classes::{Answer, ClassSpec, MemberSpec, Tier};
use super::{HostCall, Jni};

/// The Java class the protocol's static natives are declared on.
pub const PROTOCOL_CLASS: &str = "com/roblox/protocols/webview/WebViewProtocol";
/// The message bus, whose natives are **instance** methods of its one object (`MessageBus$d.a`).
pub const MESSAGE_BUS_CLASS: &str = "com/roblox/universalapp/messagebus/MessageBus";
/// `MessageBus$a`: the `RawCallback` `MessageBus.g` wraps a `Callback` in.
pub const RAW_CALLBACK_CLASS: &str = "com/roblox/universalapp/messagebus/MessageBus$a";
/// `MessageBus$b`: the `RequestHandlerRaw` `MessageBus.j` wraps a request handler in.
pub const REQUEST_HANDLER_CLASS: &str = "com/roblox/universalapp/messagebus/MessageBus$b";
/// What `doSubscribeRaw` returns, constructed by the engine with `<init>(J)V`.
pub const BUS_CONNECTION_CLASS: &str = "com/roblox/universalapp/messagebus/Connection";
/// The engine's key-value store, whose natives are static.
pub const MEMSTORAGE_CLASS: &str = "com/roblox/engine/jni/memstorage/MemStorage";
/// What `MemStorage.bind` returns, constructed by the engine with `<init>(J)V`.
pub const MEMSTORAGE_CONNECTION_CLASS: &str = "com/roblox/engine/jni/memstorage/Connection";

/// `fh.c$a`, bound to `BrowserService.OpenBrowserWindow`.
const OPEN_BROWSER_CALLBACK: &str = "fh/c$a";
/// `fh.c$b`, bound to `BrowserService.CloseBrowserWindow`.
const CLOSE_BROWSER_CALLBACK: &str = "fh/c$b";
/// `fh.c$c`, bound to `BrowserService.SendCommand`.
const SEND_COMMAND_CALLBACK: &str = "fh/c$c";
/// `ri.a$a`, bound to `BrowserService.ExecuteJavaScript` while a web view is up.
const EXECUTE_SCRIPT_CALLBACK: &str = "ri/a$a";

/// The name `cl.d.d` gives the bridge object in `addJavascriptInterface`.
pub const BRIDGE_NAME: &str = "__globalRobloxAndroidBridge__";
/// The bridge object's one method (`cl.d$c`, `@JavascriptInterface`).
pub const BRIDGE_METHOD: &str = "executeRoblox";

/// Guest instructions one call into the engine may take -- a publish or a subscription, which
/// post and return. The figure the other UI-thread calls use, for D16's reason.
const PER_CALL: omni_cpu::RunLimit = super::input::PER_EVENT;

/// What the engine's calls into the Java side's objects are, by the tag each object was made
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum Tag {
    IsAvailable = 1,
    OpenWindow = 2,
    MutateWindow = 3,
    CloseWindow = 4,
    OpenBrowserWindow = 5,
    CloseBrowserWindow = 6,
    SendCommand = 7,
    ExecuteJavaScript = 8,
}

impl Tag {
    fn of(tag: u32) -> Option<Tag> {
        [
            Tag::IsAvailable,
            Tag::OpenWindow,
            Tag::MutateWindow,
            Tag::CloseWindow,
            Tag::OpenBrowserWindow,
            Tag::CloseBrowserWindow,
            Tag::SendCommand,
            Tag::ExecuteJavaScript,
        ]
        .into_iter()
        .find(|known| *known as u32 == tag)
    }
}

const fn method(name: &'static str, descriptor: &'static str, answer: Answer) -> MemberSpec {
    MemberSpec { name, descriptor, is_static: false, answer }
}

const RAW_RUN: &[MemberSpec] = &[method("run", "(Ljava/lang/String;)V", Answer::HostCallback)];
const REQUEST_RUN: &[MemberSpec] =
    &[method("run", "(Ljava/lang/String;)Ljava/lang/String;", Answer::HostRequest)];
const ON_ITEM_SET: &[MemberSpec] = &[method("onItemSet", "(Ljava/lang/String;)V", Answer::HostCallback)];

/// The classes the Java side's objects are instances of, with the one method the engine calls on
/// each (`GetObjectClass` then `GetMethodID(.., "run" | "onItemSet", ..)` -- both names and both
/// descriptors are `.rodata` strings), and the classes whose static natives the host calls.
const CLASSES: &[ClassSpec] = &[
    ClassSpec { name: PROTOCOL_CLASS, tier: Tier::Support, methods: &[], fields: &[] },
    ClassSpec { name: MESSAGE_BUS_CLASS, tier: Tier::Support, methods: &[], fields: &[] },
    ClassSpec { name: RAW_CALLBACK_CLASS, tier: Tier::Support, methods: RAW_RUN, fields: &[] },
    ClassSpec { name: REQUEST_HANDLER_CLASS, tier: Tier::Support, methods: REQUEST_RUN, fields: &[] },
    ClassSpec { name: MEMSTORAGE_CLASS, tier: Tier::Support, methods: &[], fields: &[] },
    ClassSpec { name: OPEN_BROWSER_CALLBACK, tier: Tier::Support, methods: ON_ITEM_SET, fields: &[] },
    ClassSpec { name: CLOSE_BROWSER_CALLBACK, tier: Tier::Support, methods: ON_ITEM_SET, fields: &[] },
    ClassSpec { name: SEND_COMMAND_CALLBACK, tier: Tier::Support, methods: ON_ITEM_SET, fields: &[] },
    ClassSpec { name: EXECUTE_SCRIPT_CALLBACK, tier: Tier::Support, methods: ON_ITEM_SET, fields: &[] },
];

/// Declare [`CLASSES`], and decide the two `Connection` constructors the engine calls: each is
/// `Object.<init>` and one `iput-wide` of its `long` -- `messagebus.Connection.a`,
/// `memstorage.Connection.ref`.
///
/// A class already declared is left alone, as [`super::script::declare_script_classes`] does.
///
/// # Errors
///
/// A constructor the generated surface does not declare, naming it.
pub fn declare_classes(jni: &Jni) -> AbiResult<()> {
    jni.with_registry(|registry| {
        for spec in CLASSES {
            if registry.find(spec.name).is_none() {
                registry.declare(spec)?;
            }
        }
        Ok::<(), AbiError>(())
    })?;
    jni.define(BUS_CONNECTION_CLASS, "<init>", "(J)V", false, Answer::Construct(&[("a", "J")]))?;
    jni.define(MEMSTORAGE_CONNECTION_CLASS, "<init>", "(J)V", false, Answer::Construct(&[("ref", "J")]))?;
    Ok(())
}

/// The exported natives this module calls, by their short manglings.
mod symbols {
    pub const PROTOCOL: &str = "Java_com_roblox_protocols_webview_WebViewProtocol_";
    pub const GET_MESSAGE_ID: &str = "Java_com_roblox_universalapp_messagebus_MessageBus_getMessageId";
    pub const SET_REQUEST_HANDLER_RAW: &str =
        "Java_com_roblox_universalapp_messagebus_MessageBus_setRequestHandlerRaw";
    pub const DO_SUBSCRIBE_RAW: &str = "Java_com_roblox_universalapp_messagebus_MessageBus_doSubscribeRaw";
    pub const PUBLISH_RAW: &str = "Java_com_roblox_universalapp_messagebus_MessageBus_publishRaw";
    pub const MEMSTORAGE_BIND: &str = "Java_com_roblox_engine_jni_memstorage_MemStorage_bind";
    pub const MEMSTORAGE_DISCONNECT: &str = "Java_com_roblox_engine_jni_memstorage_Connection_disconnect";
}

/// The names the protocol's static getters answer. The Java side calls them where it needs one;
/// they return constants, so they are called once, at [`WebViewProtocol::install`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtocolNames {
    /// `getProtocolName`.
    pub protocol: String,
    /// `getIsAvailableId`: the method the request handler is registered for.
    pub is_available: String,
    /// `getMessageId(protocol, getOpenWindowId())`.
    pub open_window: String,
    /// `getMessageId(protocol, getMutateWindowId())`.
    pub mutate_window: String,
    /// `getMessageId(protocol, getCloseWindowId())`.
    pub close_window: String,
    /// `getMessageId(protocol, getHandleWindowCloseId())`.
    pub handle_window_close: String,
    /// `getAvailableKey`.
    pub available_key: String,
    /// `getUrlKey`.
    pub url_key: String,
    /// `getTitleKey`.
    pub title_key: String,
    /// `getIsVisibleKey`.
    pub is_visible_key: String,
}

/// A browser window the host opens for the Java side's web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRequest {
    /// What `loadUrl` is handed.
    pub url: String,
    /// The window's title: the fragment's header text.
    pub title: String,
    /// `WebSettings.setUserAgentString`: [`user_agent`].
    pub user_agent: String,
    /// Script that must run in every document before the page's own: the bridge object
    /// ([`bridge_script`]).
    pub init_script: String,
}

/// What a host browser window reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    /// A navigation finished -- `WebViewClient.onPageFinished`, which Android calls on failure too.
    PageFinished {
        /// The URL.
        url: String,
        /// Whether it loaded.
        success: bool,
    },
    /// The bridge object's method was called with this string.
    Bridge(String),
    /// The person closed the window.
    Closed,
    /// The browser failed, and the window is gone.
    Failed(String),
}

/// A host browser window.
pub trait BrowserWindow {
    /// What happened since the last call. Never blocks.
    fn poll(&mut self) -> Vec<BrowserEvent>;
    /// `evaluateJavascript(script, null)`.
    ///
    /// # Errors
    ///
    /// Why the script could not be handed to the page.
    fn execute_script(&mut self, script: &str) -> Result<(), String>;
    /// Close the window. Idempotent.
    fn close(&mut self);
}

/// The host's browser: opens a window per web view.
pub trait BrowserHost {
    /// Open a window and start loading the request's URL.
    ///
    /// # Errors
    ///
    /// Why no window could be opened.
    fn open(&mut self, request: &BrowserRequest) -> Result<Box<dyn BrowserWindow>, String>;
}

/// What the Java side does in answer to the engine or the page: the decisions, before any of
/// them touches the engine or the host. See [`Protocol`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Put the fragment up: open a browser window.
    Open(BrowserRequest),
    /// Take it down.
    CloseWindow,
    /// `evaluateJavascript` in the page.
    ExecuteScript(String),
    /// `MemStorage.bind("BrowserService.ExecuteJavaScript", ri.a$a)`: `ri.a.h()`.
    BindExecuteScript,
    /// Its `Connection.disconnect()`: `ri.a.i()`.
    UnbindExecuteScript,
    /// `WebViewProtocol.signalJavascriptCallback(string)`.
    Signal(String),
    /// `WebViewProtocol.v()`: `publishRaw(handleWindowClose, "{}")`.
    PublishWindowClose,
    /// What the Java side logs.
    Log(String),
    /// Something a device does that this host does not, named.
    Refuse(String),
}

/// The fragment `jk.a0.g` puts up, as far as the engine can tell it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fragment {
    /// Whether the host's window is still up: the person may close it before the engine does.
    window: bool,
    /// `ri.a.g`: set by the first `onPageFinished`, after which script runs at once.
    loaded: bool,
    /// `ri.a.f`: script the engine sent before that.
    queued: Vec<String>,
    /// `com.roblox.client.c.X0`, which `openWindow` and `mutateWindow` set.
    visible: bool,
}

/// **The Java side's decisions**, as a state machine with no engine and no window in it: calls in,
/// [`Action`]s out. [`WebViewProtocol`] carries the actions out.
#[derive(Debug, Clone, Default)]
pub struct Protocol {
    names: ProtocolNames,
    user_agent: String,
    fragment: Option<Fragment>,
}

impl Protocol {
    /// A protocol answering with `names`, whose web views present `user_agent`.
    #[must_use]
    pub fn new(names: ProtocolNames, user_agent: String) -> Self {
        Self { names, user_agent, fragment: None }
    }

    /// The names it answers with.
    #[must_use]
    pub fn names(&self) -> &ProtocolNames {
        &self.names
    }

    /// Whether a fragment is up.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.fragment.is_some()
    }

    /// `WebViewProtocol$a.a`'s answer to `isAvailable`: `{<available key>: true}`, as
    /// `JSONObject.toString` writes it.
    #[must_use]
    pub fn availability(&self) -> String {
        format!("{{{}:true}}", json::quote(&self.names.available_key))
    }

    /// What the Java side does with one call the engine made into its objects.
    #[must_use]
    pub fn on_call(&mut self, call: &HostCall) -> Vec<Action> {
        let Some(tag) = Tag::of(call.tag) else {
            return vec![Action::Refuse(format!("a call into an object with the unknown tag {}", call.tag))];
        };
        let text = call.argument.clone().unwrap_or_default();
        match tag {
            // `MessageBus$b.run` has already answered; the request's content is not read.
            Tag::IsAvailable => vec![Action::Log("isAvailable: answered available".to_string())],
            Tag::OpenWindow => self.open_window(&text),
            Tag::MutateWindow => self.mutate_window(&text),
            Tag::CloseWindow | Tag::CloseBrowserWindow => self.close_window(),
            Tag::OpenBrowserWindow => vec![Action::Refuse(
                "BrowserService.OpenBrowserWindow: `fh.c$a` builds `vl.a(url)`, whose `visible` is \
                 null, and `jk.a0$b.a` throws NullPointerException on it -- this app version \
                 cannot open a window that way"
                    .to_string(),
            )],
            Tag::SendCommand => {
                let command = match json::parse(&text) {
                    Ok(value) => value.string("command").unwrap_or_else(|| "<none>".to_string()),
                    Err(error) => format!("<not JSON: {error}>"),
                };
                vec![Action::Refuse(format!(
                    "BrowserService.SendCommand `{command}`: `fh.c$c`'s commands are not decoded"
                ))]
            }
            Tag::ExecuteJavaScript => match self.fragment.as_mut() {
                // `ri.a.b`: at once once a page has finished, queued before.
                Some(fragment) if fragment.loaded && fragment.window => vec![Action::ExecuteScript(text)],
                Some(fragment) if fragment.window => {
                    fragment.queued.push(text);
                    Vec::new()
                }
                // The binding is the web view's; with no window nothing can run it.
                _ => vec![Action::Log(format!(
                    "BrowserService.ExecuteJavaScript with no page up: {} chars not run",
                    text.chars().count()
                ))],
            },
        }
    }

    /// `WebViewProtocol$b.a`, then `jk.a0.g` and the fragment's `J0`.
    fn open_window(&mut self, text: &str) -> Vec<Action> {
        // `MessageBus$a.run`: `new JSONObject(json)`; a parse error is logged and dropped.
        let message = match json::parse(text) {
            Ok(json::Value::Object(members)) => json::Value::Object(members),
            Ok(_) | Err(_) => {
                return vec![Action::Log(
                    "Serializing message params in Do Subscribe failed: openWindow is not a JSON object"
                        .to_string(),
                )]
            }
        };
        let Some(url) = message.string(&self.names.url_key) else {
            return vec![Action::Log("Attempted to open WebView window with no URL".to_string())];
        };
        // `com.roblox.protocols.webview.a.a`: a null title is "", a null visibility is `true`.
        let title = message.string(&self.names.title_key).unwrap_or_default();
        let visible = message.boolean(&self.names.is_visible_key).unwrap_or(true);
        let mut actions = Vec::new();
        // `jk.a0.g` *replaces* whatever fragment is up (`FragmentTransaction.o`), and the old one's
        // view is destroyed.
        if self.fragment.take().is_some() {
            actions.push(Action::CloseWindow);
            actions.push(Action::UnbindExecuteScript);
        }
        if !visible {
            actions.push(Action::Log(
                "the engine asked for a hidden web view (VISIBLE false); this host has no hidden \
                 window, so it is shown"
                    .to_string(),
            ));
        }
        actions.push(Action::Open(BrowserRequest {
            url,
            title,
            user_agent: self.user_agent.clone(),
            init_script: bridge_script(),
        }));
        // `c.J0` calls `ri.a.h()` after `loadUrl`.
        actions.push(Action::BindExecuteScript);
        self.fragment = Some(Fragment { window: true, loaded: false, queued: Vec::new(), visible });
        actions
    }

    /// `WebViewProtocol$c.a` -> `c.H3`: the visibility, stored; nothing else is used.
    fn mutate_window(&mut self, text: &str) -> Vec<Action> {
        let visible = json::parse(text).ok().and_then(|message| message.boolean(&self.names.is_visible_key));
        match (self.fragment.as_mut(), visible) {
            (Some(fragment), Some(visible)) => {
                fragment.visible = visible;
                vec![Action::Log(format!("mutateWindow: visible {visible}"))]
            }
            (Some(_), None) => vec![Action::Log("mutateWindow: no visibility, nothing changes".to_string())],
            // `jk.a0.e` finds no fragment and does nothing.
            (None, _) => Vec::new(),
        }
    }

    /// `jk.a0.d`: the fragment popped and, if there was one, `handleWindowClose`.
    fn close_window(&mut self) -> Vec<Action> {
        match self.fragment.take() {
            Some(fragment) => {
                let mut actions = Vec::new();
                if fragment.window {
                    actions.push(Action::CloseWindow);
                }
                actions.push(Action::UnbindExecuteScript);
                actions.push(Action::PublishWindowClose);
                actions
            }
            None => Vec::new(),
        }
    }

    /// What the Java side does with one event of the host's window.
    #[must_use]
    pub fn on_event(&mut self, event: &BrowserEvent) -> Vec<Action> {
        let Some(fragment) = self.fragment.as_mut().filter(|fragment| fragment.window) else {
            return Vec::new();
        };
        match event {
            // `ri.a$c.onPageFinished`: `g = true`, then the queue.
            BrowserEvent::PageFinished { .. } => {
                fragment.loaded = true;
                fragment.queued.drain(..).map(Action::ExecuteScript).collect()
            }
            // `cl.d$c.executeRoblox` -> `ri.a.e` -> the listener -> `signalJavascriptCallback`.
            BrowserEvent::Bridge(text) => vec![Action::Signal(text.clone())],
            // `jk.a0.h`, with no page to go back to.
            BrowserEvent::Closed => {
                fragment.window = false;
                vec![Action::PublishWindowClose]
            }
            BrowserEvent::Failed(why) => {
                fragment.window = false;
                vec![
                    Action::Refuse(format!("the host's browser failed, and the web view is gone: {why}")),
                    Action::PublishWindowClose,
                ]
            }
        }
    }
}

/// The script that stands in for `addJavascriptInterface(cl.d$c, "__globalRobloxAndroidBridge__")`:
/// the object and its one method, whose string goes to the host through
/// `window.chrome.webview.postMessage`.
///
/// A Java `String` parameter takes a JavaScript string as it is and anything else as its string
/// form; `null` and `undefined` are refused with a `TypeError` naming this host, rather than
/// reaching the engine as a Java `null` it would convert.
#[must_use]
pub fn bridge_script() -> String {
    format!(
        r#"(() => {{
  if (window.{name}) return;
  const bridge = {{
    {method}(command) {{
      if (command === null || command === undefined) {{
        throw new TypeError("Omnidroid's {name}.{method} does not pass a null command");
      }}
      window.chrome.webview.postMessage(String(command));
    }}
  }};
  Object.defineProperty(window, "{name}", {{ value: Object.freeze(bridge), enumerable: false, configurable: false, writable: false }});
}})();"#,
        name = BRIDGE_NAME,
        method = BRIDGE_METHOD,
    )
}

/// The device facts the app's user agent is built from (`el.g.a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAgentFacts {
    /// The installed APK's `versionName` (`PackageInfo.versionName`), the `Roblox/<version>` part.
    pub app_version: String,
    /// `ActivityManager.MemoryInfo.totalMem / 1048576` (`nl.a.c`).
    pub total_memory_mb: i32,
    /// `Display.getSize` (`nl.a.j`): the app's display area in pixels.
    pub display_size: (i32, i32),
    /// `DisplayMetrics.xdpi`, `.ydpi`, truncated (`nl.a.g`).
    pub dpi: (i32, i32),
    /// `DisplayMetrics.widthPixels / density`, `heightPixels / density`, truncated (`nl.a.f`).
    pub display_dp: (i32, i32),
    /// `Build.MANUFACTURER`.
    pub manufacturer: String,
    /// `Build.MODEL`.
    pub model: String,
    /// `Build.VERSION.RELEASE`.
    pub release: String,
    /// `bh.x0.k`, the Java side's tablet flag: `InitParams.isTablet`.
    pub tablet: bool,
    /// `bh.x0.l`: ChromeOS.
    pub chrome_os: bool,
    /// `bh.x0.m`: a TV.
    pub tv: bool,
}

/// **The app's web view user agent**, `bh.x0.m` -> `el.g.a` -> `el.i.c`, DECODED:
///
/// ```text
/// Mozilla/5.0 (%dMB; %dx%d; %dx%d; %dx%d; %s; %s) %s (KHTML, like Gecko)  ROBLOX Android App %s %s Hybrid()  %s
/// ```
///
/// memory, display size, DPI, size in dp, the device name (`nl.a.b(false)`, sanitised by
/// `el.i.n`), `Build.VERSION.RELEASE`, `"AppleWebKit/537.36"`, the app version, the device kind
/// (`el.i.h`: VR, Phone, TV or Tablet), and `"%s RobloxApp/%s (%s; %s)"` over the store
/// (`"GooglePlayStore"`), the version and the distribution (`"GlobalDist"`); `" ChromeOS"` after
/// it on ChromeOS. The double spaces are the format's. Roblox's pages read "ROBLOX Android App" and
/// "Hybrid()" from it to use the app's bridge.
///
/// This host is never the Quest build (`"questvr"` is not this APK's store), so the VR kind and
/// `nl.a.b(true)`'s device suffix do not arise.
#[must_use]
pub fn user_agent(facts: &UserAgentFacts) -> String {
    const WEBKIT: &str = "AppleWebKit/537.36";
    const DISTRIBUTION: &str = "GlobalDist";
    const STORE: &str = "GooglePlayStore";
    let version = &facts.app_version;
    // `el.i.h`: VR first, then `k` (phone: `bh.x0.p0()` is `!k`), then TV, else tablet.
    let kind = if !facts.tablet {
        "Phone"
    } else if facts.tv {
        "TV"
    } else {
        "Tablet"
    };
    let store = format!("{STORE} RobloxApp/{version} ({DISTRIBUTION}; {STORE})");
    let agent = format!(
        "Mozilla/5.0 ({}MB; {}x{}; {}x{}; {}x{}; {}; {}) {WEBKIT} (KHTML, like Gecko)  ROBLOX Android App \
         {version} {kind} Hybrid()  {store}",
        facts.total_memory_mb,
        facts.display_size.0,
        facts.display_size.1,
        facts.dpi.0,
        facts.dpi.1,
        facts.display_dp.0,
        facts.display_dp.1,
        printable(&device_name(&facts.manufacturer, &facts.model)),
        facts.release,
    );
    if facts.chrome_os {
        agent + " ChromeOS"
    } else {
        agent
    }
}

/// `nl.a.b(false)`: the model when it already starts with the maker, else maker, space, model;
/// then the first character upper-cased (`Character.toUpperCase`).
fn device_name(manufacturer: &str, model: &str) -> String {
    let name = if model.starts_with(manufacturer) { model.to_string() } else { format!("{manufacturer} {model}") };
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name,
    }
}

/// `el.i.n`: every UTF-16 unit at or below 31, or at or above 127, becomes `_`.
fn printable(text: &str) -> String {
    text.encode_utf16().map(|unit| if unit <= 31 || unit >= 127 { '_' } else { char::from(unit as u8) }).collect()
}

/// **The Java side's web view, over the engine**: [`Protocol`]'s decisions carried out with the
/// engine's natives and the host's browser, on the thread the embedding calls the lifecycle
/// natives from -- the UI thread.
pub struct WebViewProtocol {
    protocol: Protocol,
    /// `MessageBus$d.a`, the bus's one object: a global this instance keeps.
    bus: u64,
    /// `jclass`es for the static natives, taken once.
    protocol_class: u64,
    memstorage_class: u64,
    memstorage_connection_class: u64,
    signal: GuestAddr,
    publish: GuestAddr,
    bind: GuestAddr,
    disconnect: GuestAddr,
    /// `WebViewProtocol.c`/`.d`/`.e` and `fh.c.d`/`.e`/`.f`: kept, as the Java objects keep them.
    connections: Vec<u64>,
    /// `ri.a.h`: the `ExecuteJavaScript` binding of the web view that is up.
    execute_script: Option<u64>,
    window: Option<Box<dyn BrowserWindow>>,
}

impl std::fmt::Debug for WebViewProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebViewProtocol")
            .field("protocol", &self.protocol)
            .field("connections", &self.connections.len())
            .field("window", &self.window.is_some())
            .finish()
    }
}

impl WebViewProtocol {
    /// **`new WebViewProtocol(jk.a0)`, then `fh.c.c()`**, with `thread`'s `JNIEnv`. Returns the
    /// protocol and what it did, one line per call.
    ///
    /// **The caller holds the activations**, as around [`super::text::TextInput::apply`].
    ///
    /// # Errors
    ///
    /// The first export `resolve` does not know, the first call that does not return, and a call
    /// that returns with a Java exception pending -- none of which the Java constructor catches.
    pub fn install(
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        resolve: &dyn Fn(&str) -> Option<GuestAddr>,
        user_agent: String,
    ) -> AbiResult<(Self, Vec<String>)> {
        let find = |symbol: &str| {
            resolve(symbol).ok_or_else(|| AbiError::JniRefused {
                function: symbol.to_string(),
                address: 0,
                detail: format!(
                    "`{symbol}` is exported by libroblox.so on a device and nothing resolved it \
                     here, so the Java side's web view cannot be built"
                ),
            })
        };
        declare_classes(jni)?;
        let mut log = Vec::new();
        let caller = Caller { jni, boundary, thread };
        let protocol_class = jni.class_reference(PROTOCOL_CLASS)?;
        let bus_class = jni.class_reference(MESSAGE_BUS_CLASS)?;
        let memstorage_class = jni.class_reference(MEMSTORAGE_CLASS)?;
        let memstorage_connection_class = jni.class_reference(MEMSTORAGE_CONNECTION_CLASS)?;
        let getter = |cpu: &mut dyn GuestCpu, name: &str| -> AbiResult<String> {
            let symbol = format!("{}{name}", symbols::PROTOCOL);
            let returned = caller.call(cpu, &format!("WebViewProtocol.{name}"), find(&symbol)?, vec![
                GuestArg::Pointer(jni.env_for(thread)),
                GuestArg::Int(protocol_class),
            ])?;
            caller.string(returned, &format!("WebViewProtocol.{name}"))
        };
        let get_message_id = find(symbols::GET_MESSAGE_ID)?;
        let message_id = |cpu: &mut dyn GuestCpu, protocol: &str, id: &str| -> AbiResult<String> {
            let (first, second) = (jni.new_string(protocol)?, jni.new_string(id)?);
            let returned = caller.call(cpu, "MessageBus.getMessageId", get_message_id, vec![
                GuestArg::Pointer(jni.env_for(thread)),
                GuestArg::Int(bus_class),
                GuestArg::Int(first),
                GuestArg::Int(second),
            ]);
            let _ = jni.delete_local(first);
            let _ = jni.delete_local(second);
            caller.string(returned?, "MessageBus.getMessageId")
        };

        let protocol = getter(cpu, "getProtocolName")?;
        let is_available = getter(cpu, "getIsAvailableId")?;
        let names = ProtocolNames {
            open_window: {
                let id = getter(cpu, "getOpenWindowId")?;
                message_id(cpu, &protocol, &id)?
            },
            mutate_window: {
                let id = getter(cpu, "getMutateWindowId")?;
                message_id(cpu, &protocol, &id)?
            },
            close_window: {
                let id = getter(cpu, "getCloseWindowId")?;
                message_id(cpu, &protocol, &id)?
            },
            handle_window_close: {
                let id = getter(cpu, "getHandleWindowCloseId")?;
                message_id(cpu, &protocol, &id)?
            },
            available_key: getter(cpu, "getAvailableKey")?,
            url_key: getter(cpu, "getUrlKey")?,
            title_key: getter(cpu, "getTitleKey")?,
            is_visible_key: getter(cpu, "getIsVisibleKey")?,
            protocol,
            is_available,
        };
        log.push(format!(
            "WebViewProtocol names: protocol {:?}, isAvailable {:?}, open {:?}, mutate {:?}, close {:?}, \
             handleWindowClose {:?}, keys {:?}/{:?}/{:?}/{:?}",
            names.protocol,
            names.is_available,
            names.open_window,
            names.mutate_window,
            names.close_window,
            names.handle_window_close,
            names.available_key,
            names.url_key,
            names.title_key,
            names.is_visible_key
        ));
        let protocol = Protocol::new(names, user_agent);

        // `MessageBus$d.<clinit>`: the bus's one object.
        let bus = jni.promote_to_global(jni.new_object(MESSAGE_BUS_CLASS)?)?;
        let mut connections = Vec::new();

        // `MessageBus.p` -> `setRequestHandlerRaw(protocol, isAvailableId, MessageBus$b(WebViewProtocol$a))`.
        let handler =
            jni.new_host_request_handler(REQUEST_HANDLER_CLASS, Tag::IsAvailable as u32, protocol.availability())?;
        caller.with_strings(
            cpu,
            "MessageBus.setRequestHandlerRaw (WebViewProtocol.<init>)",
            find(symbols::SET_REQUEST_HANDLER_RAW)?,
            &[&protocol.names().protocol, &protocol.names().is_available],
            |strings| {
                vec![
                    GuestArg::Pointer(jni.env_for(thread)),
                    GuestArg::Int(bus),
                    GuestArg::Int(strings[0]),
                    GuestArg::Int(strings[1]),
                    GuestArg::Int(handler),
                ]
            },
        )?;
        log.push(format!("setRequestHandlerRaw({:?}, {:?}) returned", protocol.names().protocol, protocol.names().is_available));

        // `MessageBus.t(id, callback)` -> `u(id, callback, false)` -> `doSubscribeRaw(id, MessageBus$a, false)`.
        let subscribe = find(symbols::DO_SUBSCRIBE_RAW)?;
        for (id, tag) in [
            (protocol.names().open_window.clone(), Tag::OpenWindow),
            (protocol.names().mutate_window.clone(), Tag::MutateWindow),
            (protocol.names().close_window.clone(), Tag::CloseWindow),
        ] {
            let callback = jni.new_host_callback(RAW_CALLBACK_CLASS, tag as u32)?;
            let connection = caller.with_strings(
                cpu,
                "MessageBus.doSubscribeRaw (WebViewProtocol.<init>)",
                subscribe,
                &[&id],
                |strings| {
                    vec![
                        GuestArg::Pointer(jni.env_for(thread)),
                        GuestArg::Int(bus),
                        GuestArg::Int(strings[0]),
                        GuestArg::Int(callback),
                        GuestArg::Int(0),
                    ]
                },
            )?;
            connections.push(caller.keep(connection, "MessageBus.doSubscribeRaw")?);
            log.push(format!("doSubscribeRaw({id:?}) -> a Connection"));
        }

        caller.call(cpu, "WebViewProtocol.initializeAndroidWebViewProtocol", find(&format!(
            "{}initializeAndroidWebViewProtocol",
            symbols::PROTOCOL
        ))?, vec![GuestArg::Pointer(jni.env_for(thread)), GuestArg::Int(protocol_class)])?;
        log.push("initializeAndroidWebViewProtocol returned".to_string());

        // `fh.c.c()`: the three `BrowserService` bindings.
        let bind = find(symbols::MEMSTORAGE_BIND)?;
        for (key, class, tag) in [
            ("BrowserService.OpenBrowserWindow", OPEN_BROWSER_CALLBACK, Tag::OpenBrowserWindow),
            ("BrowserService.CloseBrowserWindow", CLOSE_BROWSER_CALLBACK, Tag::CloseBrowserWindow),
            ("BrowserService.SendCommand", SEND_COMMAND_CALLBACK, Tag::SendCommand),
        ] {
            let callback = jni.new_host_callback(class, tag as u32)?;
            let connection = caller.with_strings(cpu, "MemStorage.bind (fh.c.c)", bind, &[key], |strings| {
                vec![
                    GuestArg::Pointer(jni.env_for(thread)),
                    GuestArg::Int(memstorage_class),
                    GuestArg::Int(strings[0]),
                    GuestArg::Int(callback),
                ]
            })?;
            connections.push(caller.keep(connection, "MemStorage.bind")?);
            log.push(format!("MemStorage.bind({key:?}) -> a Connection"));
        }

        let installed = Self {
            protocol,
            bus,
            protocol_class,
            memstorage_class,
            memstorage_connection_class,
            signal: find(&format!("{}signalJavascriptCallback", symbols::PROTOCOL))?,
            publish: find(symbols::PUBLISH_RAW)?,
            bind,
            disconnect: find(symbols::MEMSTORAGE_DISCONNECT)?,
            connections,
            execute_script: None,
            window: None,
        };
        Ok((installed, log))
    }

    /// The decisions.
    #[must_use]
    pub fn protocol(&self) -> &Protocol {
        &self.protocol
    }

    /// Whether a browser window is up.
    #[must_use]
    pub fn window_open(&self) -> bool {
        self.window.is_some()
    }

    /// `MessageBus.publishRaw(id, json)` on the bus's one object -- what `MessageBus.l` does for any
    /// Java publisher. For an embedding's **probe**: publishing `openWindow` stands in for the
    /// engine's side of a message, and the engine's own bus delivers it to the subscriptions
    /// [`WebViewProtocol::install`] made.
    ///
    /// **The caller holds the activations.**
    ///
    /// # Errors
    ///
    /// The call not returning, or returning with an exception pending.
    pub fn publish_raw(
        &self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        id: &str,
        json: &str,
    ) -> AbiResult<()> {
        let caller = Caller { jni, boundary, thread };
        let bus = self.bus;
        caller.with_strings(cpu, "MessageBus.publishRaw (a probe)", self.publish, &[id, json], |strings| {
            vec![
                GuestArg::Pointer(jni.env_for(thread)),
                GuestArg::Int(bus),
                GuestArg::Int(strings[0]),
                GuestArg::Int(strings[1]),
            ]
        })?;
        Ok(())
    }

    /// **One turn of the UI thread**: the engine's calls since the last turn, then the window's
    /// events, each carried out. Returns what happened, one line each; page content and scripts
    /// are logged by length only.
    ///
    /// **The caller holds the activations.**
    ///
    /// # Errors
    ///
    /// The first call into the engine that does not return, or returns with an exception pending.
    pub fn pump(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        host: &mut dyn BrowserHost,
    ) -> AbiResult<Vec<String>> {
        let mut log = Vec::new();
        for call in jni.take_host_calls() {
            let actions = self.protocol.on_call(&call);
            self.carry_out(jni, boundary, cpu, thread, host, actions, &mut log)?;
        }
        let events = self.window.as_mut().map(|window| window.poll()).unwrap_or_default();
        for event in events {
            match &event {
                BrowserEvent::Bridge(text) => log.push(format!("page: {BRIDGE_METHOD}(<{} chars>)", text.chars().count())),
                BrowserEvent::PageFinished { url, success } => {
                    log.push(format!("page finished: {} ({})", without_query(url), if *success { "loaded" } else { "failed" }))
                }
                BrowserEvent::Closed => log.push("the person closed the web view".to_string()),
                BrowserEvent::Failed(_) => {}
            }
            let closes = matches!(event, BrowserEvent::Closed | BrowserEvent::Failed(_));
            let actions = self.protocol.on_event(&event);
            if closes {
                if let Some(mut window) = self.window.take() {
                    window.close();
                }
            }
            self.carry_out(jni, boundary, cpu, thread, host, actions, &mut log)?;
        }
        Ok(log)
    }

    #[allow(clippy::too_many_arguments)]
    fn carry_out(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        host: &mut dyn BrowserHost,
        actions: Vec<Action>,
        log: &mut Vec<String>,
    ) -> AbiResult<()> {
        let caller = Caller { jni, boundary, thread };
        for action in actions {
            match action {
                Action::Open(request) => {
                    log.push(format!("openWindow: {} ({:?})", without_query(&request.url), request.title));
                    match host.open(&request) {
                        Ok(window) => self.window = Some(window),
                        Err(error) => {
                            log.push(format!("REFUSED: the host could not open a web view: {error}"));
                            // The fragment is up on a device; here the window it would show is
                            // not, and the page can never answer. The engine is told nothing.
                        }
                    }
                }
                Action::CloseWindow => {
                    if let Some(mut window) = self.window.take() {
                        window.close();
                        log.push("the web view closed".to_string());
                    }
                }
                Action::ExecuteScript(script) => {
                    let outcome = match self.window.as_mut() {
                        Some(window) => window.execute_script(&script),
                        None => Err("no window".to_string()),
                    };
                    match outcome {
                        Ok(()) => log.push(format!("executeJavaScript: <{} chars>", script.chars().count())),
                        Err(error) => log.push(format!("REFUSED: executeJavaScript not run: {error}")),
                    }
                }
                Action::BindExecuteScript => {
                    let callback = jni.new_host_callback(EXECUTE_SCRIPT_CALLBACK, Tag::ExecuteJavaScript as u32)?;
                    let memstorage_class = self.memstorage_class;
                    let connection = caller.with_strings(
                        cpu,
                        "MemStorage.bind (ri.a.h)",
                        self.bind,
                        &["BrowserService.ExecuteJavaScript"],
                        |strings| {
                            vec![
                                GuestArg::Pointer(jni.env_for(thread)),
                                GuestArg::Int(memstorage_class),
                                GuestArg::Int(strings[0]),
                                GuestArg::Int(callback),
                            ]
                        },
                    )?;
                    self.execute_script = Some(caller.keep(connection, "MemStorage.bind")?);
                    log.push("MemStorage.bind(\"BrowserService.ExecuteJavaScript\") -> a Connection".to_string());
                }
                Action::UnbindExecuteScript => {
                    if let Some(connection) = self.execute_script.take() {
                        caller.call(cpu, "memstorage.Connection.disconnect (ri.a.i)", self.disconnect, vec![
                            GuestArg::Pointer(jni.env_for(thread)),
                            GuestArg::Int(self.memstorage_connection_class),
                            GuestArg::Int(connection),
                        ])?;
                        log.push("BrowserService.ExecuteJavaScript disconnected".to_string());
                    }
                }
                Action::Signal(text) => {
                    let protocol_class = self.protocol_class;
                    caller.with_strings(
                        cpu,
                        "WebViewProtocol.signalJavascriptCallback (ri.a.e -> jk.a0.f)",
                        self.signal,
                        &[&text],
                        |strings| {
                            vec![
                                GuestArg::Pointer(jni.env_for(thread)),
                                GuestArg::Int(protocol_class),
                                GuestArg::Int(strings[0]),
                            ]
                        },
                    )?;
                    log.push(format!("signalJavascriptCallback(<{} chars>) returned", text.chars().count()));
                }
                Action::PublishWindowClose => {
                    let bus = self.bus;
                    let id = self.protocol.names().handle_window_close.clone();
                    caller.with_strings(cpu, "MessageBus.publishRaw (WebViewProtocol.v)", self.publish, &[&id, "{}"], |strings| {
                        vec![
                            GuestArg::Pointer(jni.env_for(thread)),
                            GuestArg::Int(bus),
                            GuestArg::Int(strings[0]),
                            GuestArg::Int(strings[1]),
                        ]
                    })?;
                    log.push(format!("publishRaw({id:?}, \"{{}}\") returned"));
                }
                Action::Log(line) => log.push(line),
                Action::Refuse(line) => log.push(format!("REFUSED: {line}")),
            }
        }
        Ok(())
    }
}

/// A URL with its query and fragment cut off, for a log line: a challenge's query carries ids.
fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Calls into the engine from the host's Java side, on one thread.
struct Caller<'a> {
    jni: &'a Jni,
    boundary: &'a Arc<Boundary>,
    thread: usize,
}

impl Caller<'_> {
    /// Make one call; what it returned in `X0`. A pending exception after it is this call's
    /// failure: the Java callers here do not catch.
    fn call(&self, cpu: &mut dyn GuestCpu, what: &str, target: GuestAddr, args: Vec<GuestArg>) -> AbiResult<u64> {
        let returned = self.boundary.call_guest(cpu, what, target, &args, PER_CALL)?;
        if let Some(exception) = self.jni.take_pending_exception(self.thread) {
            return Err(AbiError::JniRefused {
                function: what.to_string(),
                address: target,
                detail: format!("returned with a Java exception pending, which its Java caller does not catch: {exception}"),
            });
        }
        Ok(returned.x0)
    }

    /// Make one call with `texts` as fresh `String` locals, deleted once it returns.
    fn with_strings(
        &self,
        cpu: &mut dyn GuestCpu,
        what: &str,
        target: GuestAddr,
        texts: &[&str],
        args: impl FnOnce(&[u64]) -> Vec<GuestArg>,
    ) -> AbiResult<u64> {
        let mut strings = Vec::with_capacity(texts.len());
        for text in texts {
            strings.push(self.jni.new_string(text)?);
        }
        let returned = self.call(cpu, what, target, args(&strings));
        for string in strings {
            // A native may delete a local it was handed, which JNI allows.
            let _ = self.jni.delete_local(string);
        }
        returned
    }

    /// A `String` a call returned, read and its local deleted. `null` refuses: every getter here
    /// returns a constant, and a `null` one would be a name nothing can be registered under.
    fn string(&self, handle: u64, what: &str) -> AbiResult<String> {
        let text = self.jni.string_of(handle)?;
        let _ = self.jni.delete_local(handle);
        text.ok_or_else(|| AbiError::JniRefused {
            function: what.to_string(),
            address: 0,
            detail: "returned null where the Java side uses the string as a name".to_string(),
        })
    }

    /// An object a call returned, kept as the Java field that stores it keeps it. `null` refuses:
    /// the Java side stores it and later calls it.
    fn keep(&self, handle: u64, what: &str) -> AbiResult<u64> {
        if handle == 0 {
            return Err(AbiError::JniRefused {
                function: what.to_string(),
                address: 0,
                detail: "returned a null Connection, which the Java side keeps and disconnects later".to_string(),
            });
        }
        self.jni.promote_to_global(handle)
    }
}

/// The JSON the Java side reads and writes, as Android's `org.json` does it -- only as far as
/// these messages need.
pub mod json {
    /// A parsed value. Members keep their order; a repeated key's **last** value wins, as
    /// `JSONObject.put` overwrites.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        /// `JSONObject.NULL`.
        Null,
        /// A boolean.
        Bool(bool),
        /// A number, as written.
        Number(String),
        /// A string.
        String(String),
        /// An array.
        Array(Vec<Value>),
        /// An object.
        Object(Vec<(String, Value)>),
    }

    impl Value {
        fn member(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Object(members) => members.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value),
                _ => None,
            }
        }

        /// `has(key) ? getString(key) : null`: a string as it is, and any other value but an
        /// object or array as its text (`JSON.toString`: `String.valueOf`).
        #[must_use]
        pub fn string(&self, key: &str) -> Option<String> {
            match self.member(key)? {
                Value::String(text) => Some(text.clone()),
                Value::Number(text) => Some(text.clone()),
                Value::Bool(value) => Some(value.to_string()),
                Value::Null => Some("null".to_string()),
                Value::Array(_) | Value::Object(_) => None,
            }
        }

        /// `has(key) ? getBoolean(key) : null`: a boolean, or the strings `"true"`/`"false"` in
        /// any case; anything else is a type mismatch, which the Java side logs and reads as null.
        #[must_use]
        pub fn boolean(&self, key: &str) -> Option<bool> {
            match self.member(key)? {
                Value::Bool(value) => Some(*value),
                Value::String(text) if text.eq_ignore_ascii_case("true") => Some(true),
                Value::String(text) if text.eq_ignore_ascii_case("false") => Some(false),
                _ => None,
            }
        }
    }

    /// `JSONStringer.string`: quotes, backslash and **every** `/` escaped, the five short control
    /// escapes, and any other unit at or below `0x1f` as `\uXXXX`.
    #[must_use]
    pub fn quote(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for ch in text.chars() {
            match ch {
                '"' | '\\' | '/' => {
                    out.push('\\');
                    out.push(ch);
                }
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\u{c}' => out.push_str("\\f"),
                ch if (ch as u32) <= 0x1f => out.push_str(&format!("\\u{:04x}", ch as u32)),
                ch => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    /// Parse one JSON value, and nothing after it but whitespace.
    ///
    /// # Errors
    ///
    /// Where it stopped, and why.
    pub fn parse(text: &str) -> Result<Value, String> {
        let mut parser = Parser { chars: text.chars().collect(), at: 0 };
        let value = parser.value()?;
        parser.space();
        if parser.at != parser.chars.len() {
            return Err(format!("text after the value at {}", parser.at));
        }
        Ok(value)
    }

    struct Parser {
        chars: Vec<char>,
        at: usize,
    }

    impl Parser {
        fn space(&mut self) {
            while self.chars.get(self.at).is_some_and(|ch| ch.is_whitespace()) {
                self.at += 1;
            }
        }

        fn expect(&mut self, want: char) -> Result<(), String> {
            match self.chars.get(self.at) {
                Some(&ch) if ch == want => {
                    self.at += 1;
                    Ok(())
                }
                other => Err(format!("expected {want:?} at {}, found {other:?}", self.at)),
            }
        }

        fn value(&mut self) -> Result<Value, String> {
            self.space();
            match self.chars.get(self.at) {
                Some('{') => self.object(),
                Some('[') => self.array(),
                Some('"') => Ok(Value::String(self.string()?)),
                Some('t') => self.word("true", Value::Bool(true)),
                Some('f') => self.word("false", Value::Bool(false)),
                Some('n') => self.word("null", Value::Null),
                Some(ch) if *ch == '-' || ch.is_ascii_digit() => Ok(self.number()),
                other => Err(format!("no value at {}: {other:?}", self.at)),
            }
        }

        fn word(&mut self, word: &str, value: Value) -> Result<Value, String> {
            for want in word.chars() {
                self.expect(want)?;
            }
            Ok(value)
        }

        fn number(&mut self) -> Value {
            let start = self.at;
            while self.chars.get(self.at).is_some_and(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.' | 'e' | 'E')) {
                self.at += 1;
            }
            Value::Number(self.chars[start..self.at].iter().collect())
        }

        fn string(&mut self) -> Result<String, String> {
            self.expect('"')?;
            let mut units: Vec<u16> = Vec::new();
            loop {
                let Some(&ch) = self.chars.get(self.at) else {
                    return Err("an unterminated string".to_string());
                };
                self.at += 1;
                match ch {
                    '"' => break,
                    '\\' => {
                        let Some(&escape) = self.chars.get(self.at) else {
                            return Err("an unterminated escape".to_string());
                        };
                        self.at += 1;
                        match escape {
                            '"' | '\\' | '/' => units.push(escape as u16),
                            'b' => units.push(0x8),
                            'f' => units.push(0xc),
                            'n' => units.push(0xa),
                            'r' => units.push(0xd),
                            't' => units.push(0x9),
                            'u' => {
                                let hex: String = self.chars.get(self.at..self.at + 4).unwrap_or(&[]).iter().collect();
                                let unit = u16::from_str_radix(&hex, 16)
                                    .map_err(|_| format!("a bad \\u escape at {}", self.at))?;
                                self.at += 4;
                                units.push(unit);
                            }
                            other => return Err(format!("an unknown escape \\{other}")),
                        }
                    }
                    ch => {
                        let mut buffer = [0u16; 2];
                        units.extend_from_slice(ch.encode_utf16(&mut buffer));
                    }
                }
            }
            Ok(String::from_utf16_lossy(&units))
        }

        fn array(&mut self) -> Result<Value, String> {
            self.expect('[')?;
            let mut items = Vec::new();
            self.space();
            if self.chars.get(self.at) == Some(&']') {
                self.at += 1;
                return Ok(Value::Array(items));
            }
            loop {
                items.push(self.value()?);
                self.space();
                match self.chars.get(self.at) {
                    Some(',') => self.at += 1,
                    Some(']') => {
                        self.at += 1;
                        return Ok(Value::Array(items));
                    }
                    other => return Err(format!("expected , or ] at {}, found {other:?}", self.at)),
                }
            }
        }

        fn object(&mut self) -> Result<Value, String> {
            self.expect('{')?;
            let mut members = Vec::new();
            self.space();
            if self.chars.get(self.at) == Some(&'}') {
                self.at += 1;
                return Ok(Value::Object(members));
            }
            loop {
                self.space();
                let key = self.string()?;
                self.space();
                self.expect(':')?;
                let value = self.value()?;
                members.push((key, value));
                self.space();
                match self.chars.get(self.at) {
                    Some(',') => self.at += 1,
                    Some('}') => {
                        self.at += 1;
                        return Ok(Value::Object(members));
                    }
                    other => return Err(format!("expected , or }} at {}, found {other:?}", self.at)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> ProtocolNames {
        ProtocolNames {
            protocol: "WebViewProtocol".to_string(),
            is_available: "isAvailable".to_string(),
            open_window: "WebViewProtocol.openWindow".to_string(),
            mutate_window: "WebViewProtocol.mutateWindow".to_string(),
            close_window: "WebViewProtocol.closeWindow".to_string(),
            handle_window_close: "WebViewProtocol.handleWindowClose".to_string(),
            available_key: "isAvailable".to_string(),
            url_key: "url".to_string(),
            title_key: "title".to_string(),
            is_visible_key: "isVisible".to_string(),
        }
    }

    fn call(tag: Tag, argument: &str) -> HostCall {
        HostCall { tag: tag as u32, argument: Some(argument.to_string()) }
    }

    fn opened(protocol: &mut Protocol) -> Vec<Action> {
        protocol.on_call(&call(Tag::OpenWindow, r#"{"url":"https:\/\/www.roblox.com\/challenge?id=1","title":"Verify"}"#))
    }

    /// The user agent, for facts whose every field is distinct, against the format read out of
    /// `el.i.c` -- the two double spaces included.
    #[test]
    fn the_user_agent_is_the_apps_format_over_the_facts() {
        let facts = UserAgentFacts {
            app_version: "2.739.691".to_string(),
            total_memory_mb: 3584,
            display_size: (1920, 1080),
            dpi: (144, 145),
            display_dp: (1280, 720),
            manufacturer: "micro-Star".to_string(),
            model: "unknown".to_string(),
            release: "13".to_string(),
            tablet: false,
            chrome_os: false,
            tv: false,
        };
        assert_eq!(
            user_agent(&facts),
            "Mozilla/5.0 (3584MB; 1920x1080; 144x145; 1280x720; Micro-Star unknown; 13) AppleWebKit/537.36 \
             (KHTML, like Gecko)  ROBLOX Android App 2.739.691 Phone Hybrid()  GooglePlayStore \
             RobloxApp/2.739.691 (GlobalDist; GooglePlayStore)"
        );
        let tablet = UserAgentFacts { tablet: true, chrome_os: true, ..facts.clone() };
        let agent = user_agent(&tablet);
        assert!(agent.contains(" Tablet Hybrid() "), "{agent}");
        assert!(agent.ends_with("(GlobalDist; GooglePlayStore) ChromeOS"), "{agent}");
        assert!(user_agent(&UserAgentFacts { tablet: true, tv: true, ..facts }).contains(" TV Hybrid() "));
    }

    /// `nl.a.b(false)` and `el.i.n`: a model that names its maker is used alone, the first letter
    /// is upper-cased, and a unit outside 32..127 becomes `_` (one per UTF-16 unit).
    #[test]
    fn the_device_name_follows_the_java_rules() {
        assert_eq!(device_name("samsung", "SM-S918B"), "Samsung SM-S918B");
        assert_eq!(device_name("Google", "Google Pixel 8"), "Google Pixel 8");
        assert_eq!(printable("A\tB\u{7f}é😀"), "A_B____");
    }

    /// `isAvailable` answers the key the engine named, quoted as `JSONStringer` quotes it.
    #[test]
    fn availability_is_the_available_key_set_true() {
        let protocol = Protocol::new(ProtocolNames { available_key: "a/\"b".to_string(), ..names() }, String::new());
        assert_eq!(protocol.availability(), r#"{"a\/\"b":true}"#);
        assert_eq!(json::parse(&protocol.availability()).expect("JSON").boolean("a/\"b"), Some(true));
    }

    /// `openWindow` puts a window up with the URL, the title, the app's user agent and the bridge,
    /// then binds `ExecuteJavaScript` -- in that order, as `c.J0` does.
    #[test]
    fn open_window_opens_the_page_and_then_binds_script() {
        let mut protocol = Protocol::new(names(), "agent".to_string());
        let actions = opened(&mut protocol);
        assert_eq!(actions.len(), 2, "{actions:?}");
        let Action::Open(request) = &actions[0] else { panic!("{actions:?}") };
        assert_eq!(request.url, "https://www.roblox.com/challenge?id=1");
        assert_eq!(request.title, "Verify");
        assert_eq!(request.user_agent, "agent");
        assert!(request.init_script.contains(BRIDGE_NAME) && request.init_script.contains(BRIDGE_METHOD));
        assert_eq!(actions[1], Action::BindExecuteScript);
        assert!(protocol.is_open());
    }

    /// No URL is the Java side's log line and nothing else; a message that is not an object is
    /// dropped as `MessageBus$a.run` drops it.
    #[test]
    fn open_window_without_a_url_opens_nothing() {
        let mut protocol = Protocol::new(names(), String::new());
        let actions = protocol.on_call(&call(Tag::OpenWindow, r#"{"title":"x"}"#));
        assert_eq!(actions, vec![Action::Log("Attempted to open WebView window with no URL".to_string())]);
        assert!(matches!(protocol.on_call(&call(Tag::OpenWindow, "[1]"))[..], [Action::Log(_)]));
        assert!(!protocol.is_open());
    }

    /// Script the engine sends before the page has finished waits for it (`ri.a.b`), and runs at
    /// once after.
    #[test]
    fn script_waits_for_the_first_page_to_finish() {
        let mut protocol = Protocol::new(names(), String::new());
        let _ = opened(&mut protocol);
        assert!(protocol.on_call(&call(Tag::ExecuteJavaScript, "early()")).is_empty());
        let finished = protocol.on_event(&BrowserEvent::PageFinished { url: "u".to_string(), success: false });
        assert_eq!(finished, vec![Action::ExecuteScript("early()".to_string())]);
        assert_eq!(
            protocol.on_call(&call(Tag::ExecuteJavaScript, "late()")),
            vec![Action::ExecuteScript("late()".to_string())]
        );
    }

    /// What the page hands the bridge is what reaches `signalJavascriptCallback`, unchanged.
    #[test]
    fn the_bridge_string_is_signalled_unchanged() {
        let mut protocol = Protocol::new(names(), String::new());
        let _ = opened(&mut protocol);
        let command = r#"{"moduleID":"Challenge","functionName":"complete","params":{"t":"x"},"callbackID":"7"}"#;
        assert_eq!(
            protocol.on_event(&BrowserEvent::Bridge(command.to_string())),
            vec![Action::Signal(command.to_string())]
        );
    }

    /// The person closing the window publishes `handleWindowClose` once, and the fragment stays
    /// until the engine closes it, which publishes again (`jk.a0.d`) -- as a device does.
    #[test]
    fn closing_by_hand_publishes_and_the_engine_closes_the_fragment() {
        let mut protocol = Protocol::new(names(), String::new());
        let _ = opened(&mut protocol);
        assert_eq!(protocol.on_event(&BrowserEvent::Closed), vec![Action::PublishWindowClose]);
        assert!(protocol.on_event(&BrowserEvent::Bridge("late".to_string())).is_empty(), "no window, no page");
        assert!(protocol.is_open());
        assert_eq!(
            protocol.on_call(&call(Tag::CloseWindow, "{}")),
            vec![Action::UnbindExecuteScript, Action::PublishWindowClose]
        );
        assert!(!protocol.is_open());
        assert!(protocol.on_call(&call(Tag::CloseWindow, "{}")).is_empty(), "nothing up, nothing published");
    }

    /// The engine closing an open window takes it down, unbinds, then publishes.
    #[test]
    fn the_engine_closing_takes_the_window_down() {
        let mut protocol = Protocol::new(names(), String::new());
        let _ = opened(&mut protocol);
        assert_eq!(
            protocol.on_call(&call(Tag::CloseWindow, "{}")),
            vec![Action::CloseWindow, Action::UnbindExecuteScript, Action::PublishWindowClose]
        );
    }

    /// The two `BrowserService` routes this app version cannot take refuse, by name.
    #[test]
    fn the_browser_service_routes_refuse_by_name() {
        let mut protocol = Protocol::new(names(), String::new());
        let open = protocol.on_call(&call(Tag::OpenBrowserWindow, "https://x"));
        assert!(matches!(&open[..], [Action::Refuse(why)] if why.contains("NullPointerException")), "{open:?}");
        let send = protocol.on_call(&call(Tag::SendCommand, r#"{"command":"open","url":"https://x"}"#));
        assert!(matches!(&send[..], [Action::Refuse(why)] if why.contains("`open`")), "{send:?}");
        assert!(!protocol.is_open());
    }

    /// `org.json`'s reading of the values these messages carry.
    #[test]
    fn json_reads_as_org_json_does() {
        let value = json::parse(r#" {"a":"xé\/","b":true,"c":"FALSE","d":12,"e":null,"a":"last","f":{"g":[1,"2"]}} "#)
            .expect("JSON");
        assert_eq!(value.string("a").as_deref(), Some("last"), "a repeated key's last value wins");
        assert_eq!(value.boolean("b"), Some(true));
        assert_eq!(value.boolean("c"), Some(false));
        assert_eq!(value.boolean("d"), None);
        assert_eq!(value.string("d").as_deref(), Some("12"));
        assert_eq!(value.string("e").as_deref(), Some("null"));
        assert_eq!(value.string("f"), None);
        assert_eq!(value.string("missing"), None);
        assert_eq!(json::parse(r#""😀""#), Ok(json::Value::String("😀".to_string())));
        assert!(json::parse(r#"{"a":1"#).is_err());
        assert!(json::parse(r#"{"a":1} x"#).is_err());
        assert_eq!(json::quote("a\"b\\c/d\te\u{1}"), r#""a\"b\\c\/d\te\u0001""#);
    }

    /// **The answers, reached the way the engine reaches them**: `CallVoidMethodV` and
    /// `CallObjectMethodV` on objects made by `new_host_callback`/`new_host_request_handler`, with
    /// the `String` argument as the raw handle `read_varargs` passes on (VERIFICATION.md entry 20).
    #[test]
    fn a_callback_object_queues_the_call_and_a_handler_answers() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        declare_classes(&jni).expect("declared");
        let callback = jni.new_host_callback(RAW_CALLBACK_CLASS, Tag::OpenWindow as u32).expect("a callback");
        let handler = jni
            .new_host_request_handler(REQUEST_HANDLER_CLASS, Tag::IsAvailable as u32, "{\"k\":true}".to_string())
            .expect("a handler");
        let argument = jni.new_string("{\"url\":\"u\"}").expect("a string");
        let invoke = |object: u64, class: &str, descriptor: &str, function: &str| {
            let mut state = jni.state();
            let class = state.registry.find(class).expect("declared");
            let method = state.registry.method(class, "run", descriptor, false).expect("declared");
            let member = state.registry.member(method).expect("a member").clone();
            let receiver = state.handles.resolve_id("test", 0, object).expect("live");
            super::super::env::evaluate(&mut state, function, 0, class, &member, Some(receiver), &[
                super::super::values::Value::Long(argument as i64),
            ])
        };
        assert_eq!(
            invoke(callback, RAW_CALLBACK_CLASS, "(Ljava/lang/String;)V", "CallVoidMethodV").expect("answered"),
            super::super::values::Value::Void
        );
        assert_eq!(
            invoke(handler, REQUEST_HANDLER_CLASS, "(Ljava/lang/String;)Ljava/lang/String;", "CallObjectMethodV")
                .expect("answered"),
            super::super::values::Value::Text("{\"k\":true}".to_string())
        );
        assert_eq!(
            jni.take_host_calls(),
            vec![
                HostCall { tag: Tag::OpenWindow as u32, argument: Some("{\"url\":\"u\"}".to_string()) },
                HostCall { tag: Tag::IsAvailable as u32, argument: Some("{\"url\":\"u\"}".to_string()) },
            ]
        );
        assert!(jni.take_host_calls().is_empty(), "taken once");
        // An object of the class that the embedding did not make has no Java body to run.
        let stranger = jni.new_object(RAW_CALLBACK_CLASS).expect("an instance");
        assert!(invoke(stranger, RAW_CALLBACK_CLASS, "(Ljava/lang/String;)V", "CallVoidMethodV").is_err());
        // A callback asked for an answer has none.
        let as_request = {
            let mut state = jni.state();
            let class = state.registry.find(REQUEST_HANDLER_CLASS).expect("declared");
            let method = state.registry.method(class, "run", "(Ljava/lang/String;)Ljava/lang/String;", false).expect("declared");
            let member = state.registry.member(method).expect("a member").clone();
            let receiver = state.handles.resolve_id("test", 0, callback).expect("live");
            super::super::env::evaluate(&mut state, "CallObjectMethodV", 0, class, &member, Some(receiver), &[
                super::super::values::Value::Long(argument as i64),
            ])
        };
        assert!(as_request.is_err(), "{as_request:?}");
    }

    /// The two `Connection` constructors keep their `long`, which the Java side reads back.
    #[test]
    fn the_connection_constructors_keep_their_pointer() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        declare_classes(&jni).expect("declared");
        for (class, field) in [(BUS_CONNECTION_CLASS, "a"), (MEMSTORAGE_CONNECTION_CLASS, "ref")] {
            let mut state = jni.state();
            let id = state.registry.find(class).expect("declared");
            let method = state.registry.method(id, "<init>", "(J)V", false).expect("declared");
            let member = state.registry.member(method).expect("a member").clone();
            let made = super::super::env::evaluate(&mut state, "NewObjectV", 0, id, &member, None, &[
                super::super::values::Value::Long(0x7a00_1234),
            ])
            .expect("constructed");
            let super::super::values::Value::Object(Some(object)) = made else { panic!("{made:?}") };
            let field = state.registry.field(id, field, "J", false).expect("declared");
            match state.handles.object_of(object) {
                Some(super::super::refs::Object::Instance { fields, .. }) => {
                    assert_eq!(fields.get(&field), Some(&super::super::values::Value::Long(0x7a00_1234)), "{class}");
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
