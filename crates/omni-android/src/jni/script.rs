//! §8 steps 7-12: the scripted Java-side sequence, as data plus a runner.
//!
//! # The host owns the script; the engine responds to it
//!
//! `jni-surface.md` §0's structural finding, and the reason this module exists at all:
//! `libroblox.so` **will not bootstrap itself**. Flags, client settings, base URLs, directories,
//! device parameters and `InitParams` all arrive *from Java*, and `NativeEngine` waits for them.
//! On a device the caller is `RobloxApplication.onCreate`, `ActivitySplash.onCreate`, `bh.x0` and
//! `MainGameActivity`; here it is this table. Every edge in it is **VERIFIED** from dex bytecode
//! (`jni-surface-lists.txt` Section J), which is why the table carries the declaring class and
//! descriptor alongside the symbol rather than just the symbol.
//!
//! # Where it stops
//!
//! At step 12. Step 13 is `initializeNativeCode`, which needs `ALooper`, `AAssetManager` and
//! `ANativeWindow` — see this module's parent for why reaching it without a looper produces
//! §8.1's fourth failure mode, a **silent** `return 0`.
//!
//! # The name mangling is the short form, and that is measured rather than assumed
//!
//! Section G of the lists file tags each of the 706 dex natives `SHORT`, `LONG` or `REGISTER`.
//! Every method in this table is tagged **`SHORT:libroblox.so`** — a statically exported
//! `Java_<class>_<method>` with no argument suffix — so [`mangle`] is the short form and
//! `the_scripted_symbols_are_the_short_mangling_of_their_members` checks each entry against it.

use std::sync::Arc;

use omni_cpu::{GuestCpu, RunLimit};
use omni_mem::GuestAddr;

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::Jni;

/// Guest instructions one scripted downcall is allowed.
///
/// A counted budget for D16's reason, and generous: `nativeInitFastLog` starts the engine's log
/// subsystem and `nativeAppBridgeSetInitParams` walks an object graph. Comfortably below
/// `i64::MAX`, which is D16's footgun.
pub const PER_DOWNCALL: RunLimit = RunLimit::Instructions(200_000_000);

/// One argument of a scripted downcall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptArg {
    /// A `java.lang.String` built from this text.
    Text(&'static str),
    /// A fresh instance of this declared class.
    Object(&'static str),
    /// Java `null`, which several of these genuinely are before a user signs in.
    Null,
    /// A `jlong`.
    Long(i64),
}

/// One downcall in the scripted sequence.
#[derive(Debug, Clone, Copy)]
pub struct Downcall {
    /// Which §8 step it belongs to.
    pub step: u8,
    /// Who calls it on a device (`jni-surface-lists.txt` Section J).
    pub caller: &'static str,
    /// The declaring class, in JNI form.
    pub class: &'static str,
    /// The method name.
    pub member: &'static str,
    /// Its descriptor.
    pub descriptor: &'static str,
    /// The arguments after `(JNIEnv*, jclass)`.
    pub args: &'static [ScriptArg],
}

impl Downcall {
    /// The exported symbol this downcall calls.
    #[must_use]
    pub fn symbol(&self) -> String {
        mangle(self.class, self.member)
    }
}

/// The short JNI mangling: `Java_` + the class with `/` as `_`, then `_`, then the method.
///
/// The two escapes are here because a name that needed one and did not get it would resolve to
/// nothing and the step would report "no such export" rather than being wrong — but none of the
/// classes or methods in [`SEQUENCE`] contains either character, which
/// `the_scripted_symbols_are_the_short_mangling_of_their_members` also checks.
#[must_use]
pub fn mangle(class: &str, member: &str) -> String {
    fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            match ch {
                '/' | '.' => out.push('_'),
                '_' => out.push_str("_1"),
                ';' => out.push_str("_2"),
                '[' => out.push_str("_3"),
                '$' => out.push_str("_00024"),
                other => out.push(other),
            }
        }
        out
    }
    format!("Java_{}_{}", escape(class), escape(member))
}

/// The value the host tells the engine the app version is.
///
/// The APK's own: `Roblox-2.738.1397.apk`. Stated as a constant so that the one place it appears
/// is this one — three drifted duplicates of a figure have already appeared in this project.
pub const APP_VERSION: &str = "2.738.1397";

/// What the host tells the engine this *application* is called.
///
/// # This one string decided whether the client-settings fetch could ever succeed
///
/// The value used to be `"android"`, which nothing in the APK or the binary said. It made the
/// engine ask for `https://clientsettingscdn.roblox.com/v2/settings/application/android` and the
/// server answered `HTTP 400` — MEASURED, every run before this change:
///
/// ```text
/// [FLog::Output] settingsUrl: https://clientsettingscdn.roblox.com/v2/settings/application/android
/// [FLog::Error] fetch flag exception: HTTP 400
/// [FLog::NativeDM] ... getFlags: success = false.
/// ```
///
/// **The path segment is this string, decoded rather than guessed.** `0x0224ca98` builds the
/// settings URL; at `0x0224cda8` it formats `/v2/settings/application/{}` (the literal at
/// `0x0038c7a2`) with the `std::string` it was handed as its first argument, and the query it
/// passes to the URL assembler at `0x0224ce88` is the **empty** string at `0x002ece6e` — so this
/// request carries no `apiKey` and no other parameter, and the only thing in it this host chooses
/// is the application name. Its caller `0x04ecae88` is reached from `0x02bd564c`, inside the very
/// function whose success path at `0x02bd5560` runs `continueAfterFlagsLoaded_`, and it takes the
/// name from `0x04ecc164`, which returns the global `std::string` at `0x06cd4770` when that string
/// is non-empty and the compiled-in default `"AndroidApp"` at `0x006cf650` when it is not.
/// `0x06cd4770` is exactly what `Java_..._nativeOverrideChannelPlatformName` writes, through
/// `0x021f74e4`. So whatever the host passes to that downcall *is* the path segment.
///
/// **The value is the APK's own, read out of `classes2.dex`.** `bh.x0.W0` — the method
/// `jni-surface-lists.txt` Section J already names as the caller of both
/// `nativeOverrideChannelPlatformName2` and `nativeOverrideChannelPlatformName` — passes the
/// result of `bh.x0.M` to both, and `M` is one instruction long:
///
/// ```text
/// -- direct M
///   0000: const-string v0, "GoogleAndroidApp"
/// ```
///
/// That is the same class of evidence as the CA bundle: the bytes are the APK's, not this
/// project's. It is also the *right* name for this APK specifically — a Google Play build says
/// `GoogleAndroidApp`, and the binary's fallback `AndroidApp` is what a build without a Java side
/// would ask for. Passing the fallback would work against the server and would be a quieter lie
/// about which distribution this is, so the dex's constant wins.
///
/// **What would falsify this.** If the server answered `400` for `GoogleAndroidApp` too, the
/// malformed part of the request would be somewhere other than the path, and the next place to
/// look is the header map `nativeSetPlatformHeadersWithIdfa` builds — decoded at `0x02229ecc` as
/// four entries, `mdid` and `idfv` from its first argument, `asid` from its second and `idfa` from
/// its third.
pub const CHANNEL_PLATFORM_NAME: &str = "GoogleAndroidApp";

/// §8 steps 7-12, in order. Every edge is VERIFIED from dex bytecode (Section J).
pub static SEQUENCE: &[Downcall] = &[
    // ---- step 7: RobloxApplication.onCreate ----------------------------------------------
    Downcall {
        step: 7,
        caller: "com/roblox/client/RobloxApplication.onCreate",
        class: "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        args: &[ScriptArg::Object("android/app/Application")],
    },
    Downcall {
        step: 7,
        caller: "com/roblox/client/RobloxApplication.onCreate",
        class: "com/roblox/universalapp/linking/JNIWebLoginProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        args: &[ScriptArg::Object("android/app/Application")],
    },
    // ---- step 8: ActivitySplash.onCreate --------------------------------------------------
    Downcall {
        step: 8,
        caller: "com/roblox/client/startup/ActivitySplash.onCreate",
        class: "com/roblox/engine/jni/NativeReportingInterface",
        member: "initAppShellReporter",
        descriptor: "()V",
        args: &[],
    },
    // ---- step 9: the settings bootstrap, `bh.x0` -- 11 downcalls, all (String...)V --------
    //
    // **The order inside step 9 is a correction to §8, made by running it.** §8 lists
    // `nativeInitFastLog` first and the two directory calls fifth and sixth. The engine refuses
    // that order: `nativeInitFastLog` throws `Cannot initialize fastlog system.  Cache
    // directory not set.` and `raise`s SIGTRAP. MEASURED on the real binary. §8 step 9 is a
    // *list* of the eleven downcalls `bh.x0` makes and not a proof of their order -- §4.2
    // attributes them to three different methods (`W0`, `T0`, `X0`) and nothing said which of
    // the three runs first. `T0` does.
    Downcall {
        step: 9,
        caller: "bh/x0.T0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetCacheDirectory",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("/data/data/com.roblox.client/cache")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.T0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetFilesDirectory",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("/data/data/com.roblox.client/files")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeInitFastLog",
        descriptor: "()V",
        args: &[],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetRobloxVersion",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text(APP_VERSION)],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetRobloxChannel",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetBaseUrl",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;)V",
        args: &[ScriptArg::Text("https://www.roblox.com"), ScriptArg::Text("roblox.com")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetExceptionReasonFilename",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("/data/data/com.roblox.client/files/exitReason")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.X0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetPlatformHeadersWithIdfa",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        // **Three device identifiers, and this host honestly has one of them.** The arguments
        // used to be `("Android", APP_VERSION, "")`, which was three guesses in a row and
        // nothing in the APK or the binary said any of them. What the downcall actually does,
        // decoded at `0x02229ecc`: it converts its three `jstring`s and inserts four entries into
        // a map — `mdid` and `idfv` both from the **first** (`0x02229f54`, `0x02229fb4`), `asid`
        // from the second (`0x0222a014`) and `idfa` from the third (`0x0222a074`).
        //
        // `bh.x0.X0` in `classes2.dex` calls it as
        // `nativeSetPlatformHeadersWithIdfa(v0, v2, v1)` with `v0 = bh.x0.t`,
        // `v2 = "googleplay"` and `v1 = pk.c.d()`. So:
        //
        // * **first** — `bh.x0.t` is `Settings.Secure.getString(resolver, "android_id")`, read in
        //   the same method that fills `bh.x0.n`. That is the device's SSAID. **This host is not
        //   an Android device and has no `android_id`**, so the empty string is the truth about
        //   it; there is no value here that would be anything but invented.
        // * **second** — `"googleplay"`, a constant in the dex, so it is the APK's own byte.
        // * **third** — `pk.c.d()` is the Google advertising identifier, and the dex's own
        //   fallback when it cannot be had is `""`. This host cannot have one either, and the
        //   empty string was already what was passed — unchanged.
        //
        // **What would falsify the first one being empty.** A device whose user has reset or
        // withheld these identifiers sends empty strings here too, so an engine that refused to
        // work without them would not work on such a device. If a run shows a request failing
        // *because* `mdid` is empty, that assumption is wrong and the value has to come from the
        // embedding through a seam, with no default — the shape `set_filesystem_root` has.
        args: &[ScriptArg::Text(""), ScriptArg::Text("googleplay"), ScriptArg::Text("")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetUserId",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("0")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeOverrideChannelPlatformName",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text(CHANNEL_PLATFORM_NAME)],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeOverrideChannelPlatformName2",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text(CHANNEL_PLATFORM_NAME)],
    },
    // ---- step 10: MainGameActivity.b2 / .a2 -----------------------------------------------
    Downcall {
        step: 10,
        caller: "com/roblox/client/startup/MainGameActivity.b2",
        class: "com/roblox/client/startup/MainGameActivity",
        member: "nativeSetAssetPath",
        descriptor: "(Ljava/lang/String;)V",
        // **A directory, not the apk file.** MEASURED: passing the apk path made the engine
        // throw `'/data/app/com.roblox.client/base.apk' is not a directory`. §8 step 10 says
        // only "nativeSetAssetPath(String)"; what the string is was not in the analysis.
        args: &[ScriptArg::Text("/data/app/com.roblox.client")],
    },
    Downcall {
        step: 10,
        caller: "com/roblox/client/startup/MainGameActivity.a2",
        class: "com/roblox/client/startup/MainGameActivity",
        member: "nativePreloadFlagOverrides",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("")],
    },
    // ---- step 11: device info and the directories -----------------------------------------
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/MainGameActivity.E2",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetDeviceInfo",
        descriptor: "(Lcom/roblox/engine/jni/model/DeviceParams;)V",
        args: &[ScriptArg::Object("com/roblox/engine/jni/model/DeviceParams")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetExternalDirectory",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("/storage/emulated/0/Android/data/com.roblox.client")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetPreferencesFile",
        descriptor: "(Ljava/lang/String;)V",
        args: &[ScriptArg::Text("/data/data/com.roblox.client/shared_prefs/prefs.xml")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativeSetAppPreviousExitReasons",
        descriptor: "(Ljava/util/List;)V",
        args: &[ScriptArg::Object("java/util/List")],
    },
    // ---- step 12: the app-bridge init parameters ------------------------------------------
    Downcall {
        step: 12,
        caller: "com/roblox/client/startup/MainGameActivity$Companion.e",
        class: "com/roblox/client/startup/MainGameActivity",
        member: "nativeAppBridgeSetInitParams",
        descriptor: "(Lcom/roblox/engine/jni/autovalue/InitParams;)V",
        args: &[ScriptArg::Object("com/roblox/engine/jni/autovalue/InitParams")],
    },
];

/// What the host hands `nativeInitClientSettings` as the client-settings document.
///
/// # This argument is the whole of why row 21 can happen offline, and it was decoded
///
/// `Java_..._nativeInitClientSettings` (guest `0x022265fc`) converts its three `jstring`s and
/// calls `0x02baf38c(json, "", arg2, arg3)`. That function **branches on whether the first string
/// is empty**:
///
/// ```text
/// 0x2baf3f0: cbz x8, 0x2baf44c      ; length == 0 -> the fetch path
/// 0x2baf3f8: bl  ...                ; otherwise parse it in place
/// 0x2baf424: ldrb w8, [x20, #0x58]  ; the parser's error flag
/// 0x2baf428: cbz  w8, 0x2baf530     ; clear -> "ClientAppSettings", parse_ixp_cache_begin, ...
/// ```
///
/// The empty branch goes to `0x04ecae88("ClientAppSettings", ..)` with the third string, which is
/// an HTTP fetch of `clientsettings.roblox.com`; the non-empty branch parses the document the
/// caller supplied and needs no network at all. On a device the Java side fetches it and passes
/// it here, so **supplying it is what the Java side does**, not a way around the fetch.
///
/// `applicationSettings` is empty because this host has no settings document to be honest about.
/// Every flag then takes the value it was compiled with, which is a state the engine is written
/// for — it is what a device gets for any flag the response omits. Inventing flag values here
/// would be choosing engine behaviour by guess; an empty map chooses nothing.
pub const CLIENT_SETTINGS: &str = r#"{"applicationSettings":{}}"#;

/// `nativeInitClientSettings`'s **third argument, which is an application name and not a URL**.
///
/// # It was a URL, it was never reached in the run that justified it, and the engine used it anyway
///
/// This constant used to be `CLIENT_SETTINGS_URL`, holding
/// `"https://clientsettings.roblox.com/v2/settings/application/"`, with a doc comment saying it was
/// "the base URL `nativeInitClientSettings` would fetch from" and that it was "not reached". Both
/// halves were wrong, and the run said so in one line:
///
/// ```text
/// [DFLog::HttpTraceError] HttpResponse(#7) time:5012.4ms error:9 message:HttpError: Aborted url:
///   { "https://clientsettingscdn.roblox.com/v2/settings-compressed/application/
///      https:/clientsettings.roblox.com/v2/settings/application/.zst" }
/// [FLog::DynamicFastVariableReloader] Could not fetch settings
/// ```
///
/// That is `/v2/settings-compressed/application/{}.zst` — the literal at `0x003a90f7`, formatted
/// at `0x0224cd68` by the same URL builder as the plain path — with `{}` filled in by this string.
/// So the argument is the `{}`: an **application name**, the same slot
/// [`CHANNEL_PLATFORM_NAME`] fills on the other route. The parse branch not reaching
/// `0x04ecae88` was true and irrelevant; the string is kept and used later by the periodic
/// reloader regardless of which branch ran.
///
/// The decode agrees. `Java_..._nativeInitClientSettings` (`0x022265fc`) converts its three
/// `jstring`s and calls `0x02baf38c(json, "", s2, s3)`; the empty-document branch at `0x02baf44c`
/// takes the data pointer of **`s3`** at `0x02baf480`-`0x02baf490` and passes it as `0x04ecae88`'s
/// first argument at `0x02baf4d0` — the parameter that becomes the path segment.
///
/// **And the APK settles it.** `fi.e$f` in `classes2.dex`, the caller
/// `jni-surface-lists.txt` Section J names for this downcall:
///
/// ```text
///   0060: invoke {} Lbh/x0;.M
///   0063: move-result-object v5
///   0064: invoke {v0, v1, v5} NativeGLInterface.nativeInitClientSettings
/// ```
///
/// `bh.x0.M` is the one-instruction accessor that returns `"GoogleAndroidApp"`, so the third
/// argument is the same value [`CHANNEL_PLATFORM_NAME`] holds, and this is an alias rather than a
/// second copy — `APP_VERSION`'s doc records what three drifted duplicates of one figure cost.
pub const CLIENT_SETTINGS_APPLICATION: &str = CHANNEL_PLATFORM_NAME;

/// §8 rows 21-22: the client-settings phase, and the app start it unblocks.
///
/// # Why this is a second table and not more rows on [`SEQUENCE`]
///
/// [`SEQUENCE`] runs before §8 step 13; these run after §8 rows 17-20, with the game thread
/// already inside `NativeEngine::GameLoop()`. They are the same *shape* — a static native the
/// Java side calls — and run by the same [`run`], but a host that ran them at step 12 would be
/// starting the app before there was a window to start it on.
///
/// # What made them the frontier
///
/// MEASURED, after rows 17-20 landed and the engine took the window: the game thread logged
/// `[FLog::NativeDM] nativeActivity_onSurfaceChanged: ... Flags-Not-Received. Return.` That is
/// `jni-surface.md` §8.1's **sixth** failure mode arriving exactly where it says it will — the
/// engine sits waiting for flags and never asks for a renderer, which from outside looks like a
/// graphics problem and is not one.
pub static FLAGS_AND_START: &[Downcall] = &[
    // ---- row 21: the client settings, and the initialisation they unblock ------------------
    Downcall {
        step: 21,
        caller: "fi/e$f.a",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativeInitClientSettings",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
        args: &[
            ScriptArg::Text(CLIENT_SETTINGS),
            // The second string is not on either branch of `0x02baf38c`'s first test. Empty
            // rather than invented: the host has nothing to put here that it measured.
            ScriptArg::Text(""),
            ScriptArg::Text(CLIENT_SETTINGS_APPLICATION),
        ],
    },
    Downcall {
        step: 21,
        caller: "fi/e$f.b",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativePostClientSettingsLoadedInitialization3",
        descriptor: "(Ljava/util/List;)V",
        args: &[ScriptArg::Object("java/util/List")],
    },
    // ---- row 22: global init, then the app itself -------------------------------------------
    Downcall {
        step: 22,
        caller: "fi/e.E",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativeGameGlobalInit",
        descriptor: "()V",
        args: &[],
    },
];

/// The classes [`SEQUENCE`] names that §3.1 does not rank, declared so that the engine can take
/// a `jclass` for each and so that its member lookups on the parameter objects are **recorded**
/// rather than refused.
///
/// Deliberately memberless: these five are classes a `static` native is *called on*, and nothing
/// looks members up on them. The parameter objects the script passes -- `DeviceParams`,
/// `PlatformParams`, `DeviceStaticParams`, `InitParams` -- are **not** here: their member lists
/// were read out of `classes2.dex` and are declared in [`super::classes::DECLARED`], because the
/// engine reads them field by field and a missing one is one refusal per member.
pub static SCRIPT_CLASSES: &[&str] = &[
    "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
    "com/roblox/universalapp/linking/JNIWebLoginProtocol",
    "com/roblox/engine/jni/NativeReportingInterface",
    "com/roblox/engine/jni/NativeSettingsInterface",
    "com/roblox/engine/jni/NativeGLInterface",
];

/// What one step did.
#[derive(Debug)]
pub struct StepOutcome {
    /// Which §8 step.
    pub step: u8,
    /// The symbol that was called.
    pub symbol: String,
    /// Where it was, or `None` when the export was not found.
    pub target: Option<GuestAddr>,
    /// `Ok` when the guest returned through the sentinel.
    pub result: AbiResult<()>,
    /// What the guest returned in `X0`, when it returned at all.
    ///
    /// **Not every downcall here is `(...)V`.** §8 row 21's `nativeInitClientSettings` is
    /// `(SSS)I`, and a host that ignored the `int` would be discarding the engine's own report of
    /// whether the settings document it was handed was usable -- which is the difference between
    /// a run that can reach a frame and one that fatals thousands of instructions later, in a
    /// different call, with a message about something else.
    pub returned: Option<u64>,
    /// [`GuestCpu::last_run_instructions`] after the call.
    ///
    /// **The last run *segment*, not the whole downcall**, and the name says so: every exit-path
    /// crossing ends a segment and starts a new one, so a downcall that crossed the boundary ten
    /// times reports only what it executed after the tenth. The M3 gate accumulates the same
    /// quantity per initializer and has the same property. It is recorded because a segment of
    /// zero distinguishes "the export was never entered" from "it ran", not as a cost figure.
    pub last_segment_instructions: u64,
}

impl StepOutcome {
    /// Whether the step completed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// Declare the classes [`SEQUENCE`] needs into `jni`.
///
/// Idempotent in effect: a class already declared is left alone rather than reported, because a
/// host that declared one itself with real members should keep them.
pub fn declare_script_classes(jni: &Jni) {
    jni.with_registry(|registry| {
        for name in SCRIPT_CLASSES {
            if registry.find(name).is_some() {
                continue;
            }
            // `declare` only fails on a duplicate name or an unencodable id, and the duplicate is
            // excluded above. The result is dropped rather than unwrapped so that a host with
            // 65,536 classes gets a registry that is short rather than a panic.
            let _ = registry.declare(&super::classes::ClassSpec {
                name,
                tier: super::classes::Tier::Support,
                methods: &[],
                fields: &[],
            });
        }
    });
}

/// Run the scripted sequence.
///
/// `resolve` maps an exported symbol name to its guest address — the loader's export table. A
/// symbol it does not know makes that step report `target: None` and an
/// [`AbiError::JniRefused`] naming it, and the run **continues**: the point of the script is to
/// find out how far the engine gets, and stopping at the first missing export would answer a
/// different question.
///
/// Every downcall goes through [`Boundary::call_guest`], which is the host-initiated entry point
/// M3's gate added. `caller` in the errors is the §8 step and the Java method that makes the call
/// on a device, so a failure says *which* of the twenty it was without a guest address to look up.
///
/// # Errors
///
/// Never as a whole: each step's failure is in its own [`StepOutcome`]. The signature returns
/// [`AbiResult`] for the one thing that is not a step — building an argument, which means the
/// handle tables are full and no step can run.
pub fn run(
    jni: &Arc<Jni>,
    boundary: &Arc<Boundary>,
    cpu: &mut dyn GuestCpu,
    resolve: &dyn Fn(&str) -> Option<GuestAddr>,
    steps: &[Downcall],
    thread: usize,
) -> AbiResult<Vec<StepOutcome>> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        let symbol = step.symbol();
        let Some(target) = resolve(&symbol) else {
            outcomes.push(StepOutcome {
                step: step.step,
                symbol: symbol.clone(),
                target: None,
                result: Err(AbiError::JniRefused {
                    function: symbol,
                    address: 0,
                    detail: format!(
                        "`{}.{}{}` is exported by libroblox.so on a device (Section G tags it \
                         SHORT) and nothing resolved it here, so §8 step {} cannot run",
                        step.class, step.member, step.descriptor, step.step
                    ),
                }),
                returned: None,
                last_segment_instructions: 0,
            });
            continue;
        };
        // `(JNIEnv*, jclass)` then the declared arguments: every method in `SEQUENCE` is `static`,
        // which Section G states for each of them.
        let mut args = vec![
            GuestArg::Pointer(jni.env_for(thread)),
            GuestArg::Int(jni.class_reference(step.class)?),
        ];
        for argument in step.args {
            args.push(match argument {
                ScriptArg::Text(text) => GuestArg::Int(jni.new_string(text)?),
                ScriptArg::Object(class) => GuestArg::Int(jni.new_object(class)?),
                ScriptArg::Null => GuestArg::Int(0),
                ScriptArg::Long(value) => GuestArg::Int(*value as u64),
            });
        }
        let caller = format!("§8 step {} ({})", step.step, step.caller);
        let result = boundary.call_guest(cpu, &caller, target, &args, PER_DOWNCALL);
        outcomes.push(StepOutcome {
            step: step.step,
            symbol,
            target: Some(target),
            returned: result.as_ref().ok().map(|returned| returned.x0),
            result: result.map(|_| ()),
            last_segment_instructions: cpu.last_run_instructions(),
        });
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every symbol the script calls is the **short** mangling of its `(class, member)`, which is
    /// what Section G tags each of them. A name that needed an escape would be caught here.
    #[test]
    fn the_scripted_symbols_are_the_short_mangling_of_their_members() {
        for step in SEQUENCE {
            let symbol = step.symbol();
            assert!(symbol.starts_with("Java_"), "{symbol}");
            assert!(!symbol.contains('/'), "{symbol}");
            assert!(
                !symbol.contains("_1") && !symbol.contains("_00024"),
                "{symbol} needed an escape, so the short form is not what the export is called"
            );
            assert!(symbol.ends_with(step.member), "{symbol} must end with the method name");
        }
        // Four spelled out, because a mangler that produced plausible nonsense would pass the
        // shape checks above.
        assert_eq!(
            mangle("com/roblox/engine/jni/NativeSettingsInterface", "nativeSetRobloxVersion"),
            "Java_com_roblox_engine_jni_NativeSettingsInterface_nativeSetRobloxVersion"
        );
        assert_eq!(
            mangle("com/roblox/client/startup/MainGameActivity", "nativeAppBridgeSetInitParams"),
            "Java_com_roblox_client_startup_MainGameActivity_nativeAppBridgeSetInitParams"
        );
        assert_eq!(
            mangle("com/roblox/universalapp/linking/JNIBaseUrlProtocol", "init"),
            "Java_com_roblox_universalapp_linking_JNIBaseUrlProtocol_init"
        );
        assert_eq!(
            mangle("com/roblox/engine/jni/NativeReportingInterface", "initAppShellReporter"),
            "Java_com_roblox_engine_jni_NativeReportingInterface_initAppShellReporter"
        );
        // And the escapes themselves, which nothing in `SEQUENCE` exercises.
        assert_eq!(mangle("a/b_c", "d$e"), "Java_a_b_1c_d_00024e");
    }

    /// §8's own counts, per step. Stated in the table rather than remembered, because the step
    /// 9 row says "11 downcalls" and a table that had ten would still look right.
    #[test]
    fn the_sequence_has_the_shape_section_8_states() {
        let count = |step: u8| SEQUENCE.iter().filter(|d| d.step == step).count();
        assert_eq!(count(7), 2, "JNIBaseUrlProtocol.init and JNIWebLoginProtocol.init");
        assert_eq!(count(8), 1, "NativeReportingInterface.initAppShellReporter");
        assert_eq!(count(9), 11, "§8 step 9: `11 downcalls, all (String…)V`");
        assert_eq!(count(10), 2, "nativeSetAssetPath and nativePreloadFlagOverrides");
        assert_eq!(count(11), 4, "nativeSetDeviceInfo, External, Preferences, ExitReasons");
        assert_eq!(count(12), 1, "nativeAppBridgeSetInitParams");
        assert_eq!(SEQUENCE.len(), 21);
        // In order: a script that ran step 12 before step 9 would be a different script.
        for pair in SEQUENCE.windows(2) {
            assert!(pair[0].step <= pair[1].step, "the sequence must be in step order");
        }
    }

    /// Step 9's own description: "11 downcalls, **all `(String…)V`**". `nativeInitFastLog` is
    /// `()V`, which is the one the phrase glosses over — so the assertion is that every step-9
    /// parameter is a `String` and every return is `void`.
    #[test]
    fn every_step_nine_downcall_takes_only_strings_and_returns_void() {
        for step in SEQUENCE.iter().filter(|d| d.step == 9) {
            assert!(step.descriptor.ends_with(")V"), "{}", step.member);
            let parameters = &step.descriptor[1..step.descriptor.len() - 2];
            assert!(
                parameters.is_empty()
                    || parameters.split_inclusive(';').all(|p| p == "Ljava/lang/String;"),
                "{} takes {parameters}",
                step.member
            );
            for argument in step.args {
                assert!(matches!(argument, ScriptArg::Text(_)), "{}", step.member);
            }
        }
    }

    /// Every class a step names is either declared by default or in [`SCRIPT_CLASSES`]. A step
    /// whose class is in neither would fail on its `jclass` rather than on the engine.
    #[test]
    fn every_class_the_script_names_is_declarable() {
        let declared: Vec<&str> =
            super::super::classes::DECLARED.iter().map(|spec| spec.name).collect();
        for step in SEQUENCE {
            assert!(
                declared.contains(&step.class) || SCRIPT_CLASSES.contains(&step.class),
                "{} is called on an undeclared class",
                step.member
            );
            for argument in step.args {
                if let ScriptArg::Object(class) = argument {
                    assert!(
                        declared.contains(class) || SCRIPT_CLASSES.contains(class),
                        "{class} is an argument of {} and is not declared",
                        step.member
                    );
                }
            }
        }
    }
}
