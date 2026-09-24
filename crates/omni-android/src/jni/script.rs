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
//! At step 11 -- step 12 waits for the engine, see [`ENGINE_SETTINGS`]. Step 13 is
//! `initializeNativeCode`, which needs `ALooper`, `AAssetManager` and `ANativeWindow` — see this
//! module's parent for why reaching it without a looper produces §8.1's fourth failure mode, a
//! **silent** `return 0`.
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
    /// The list of how earlier runs ended -- [`super::Jni::previous_exit_reasons`], built from
    /// what the embedding recorded, and empty when it recorded nothing.
    PreviousExitReasons,
    /// `CookieManager.getCookie(url)` against the app's cookie store -- [`super::Jni::cookie_header`]
    /// -- as a `java.lang.String`: `""` when it holds none, which is what `bh.x0.S0` passes then.
    CookiesFor(&'static str),
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
    /// What the calling Java method does **before** this downcall that the engine can observe
    /// later -- run by [`run`] ahead of the call, in order. Empty for almost every row.
    pub java_before: &'static [JavaStatement],
    /// The arguments after `(JNIEnv*, jclass)`.
    pub args: &'static [ScriptArg],
}

/// A statement the app's Java code executes **between** two downcalls: `sput-object value,
/// class->field:descriptor`.
///
/// # Why the script carries Java statements at all
///
/// Nothing crosses the boundary here, so [`Downcall`] alone could not say it -- but the engine
/// reads the result back later through JNI, and the only honest source for what it reads is the
/// step that wrote it. `org.fmod.FMOD.checkInit()` is `gContext != null`, and `gContext` is
/// written by `FMOD.init(Context)`, which `NativeHelper.Q` calls between two of step 11's
/// downcalls ([`FMOD_INIT`]). Answering `checkInit` with a constant `true` would claim that
/// step had run whether or not it had; answering it from a field this statement writes makes the
/// claim true by construction, and makes a host that skips step 11 get the `false` a device with
/// no `FMOD.init` would.
///
/// The field must be declared [`Answer::Assigned`](super::classes::Answer::Assigned) --
/// [`Jni::put_static_object`] refuses anything else -- so a statement cannot overwrite a value
/// this layer answers some other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JavaStatement {
    /// Where it is in the APK -- the calling method and the dex pc -- so it can be re-read.
    pub site: &'static str,
    /// The declaring class of the field, in JNI form.
    pub class: &'static str,
    /// The static field assigned.
    pub field: &'static str,
    /// Its type descriptor.
    pub descriptor: &'static str,
    /// What is stored: an [`ScriptArg::Object`] instance, or [`ScriptArg::Null`].
    pub value: ScriptArg,
}

/// `org.fmod.FMOD.init(Context)`, where `NativeHelper.Q` calls it -- read out of `classes2.dex`:
///
/// ```text
/// NativeHelper.Q(Context, NativeHelper):
///   0004: invoke-static {v0}, NativeGLInterface.nativeSetAppPreviousExitReasons(List)
///   0021: iget-object v5, v7, NativeHelper.a:MainGameActivity
///   0023: invoke-static {v5}, Lorg/fmod/FMOD;->init(Landroid/content/Context;)V    <- no branch
///   002d: invoke-static {}, bh.x0.U0()V              (nativeSetHttpClientProxy; not scripted)
///   003a: invoke-static {v0}, NativeSettingsInterface.nativeSetPreferencesFile(String)
///   00a6: invoke-static {v4}, NativeSettingsInterface.nativeSetExternalDirectory(String)
/// FMOD.init(Context):
///   0000: sput-object v2, Lorg/fmod/FMOD;->gContext:Landroid/content/Context;
///   0002: if-eqz v2 -> return
///   0004..000f: gContext.registerReceiver(gPluginBroadcastReceiver, HEADSET_PLUG filter)
/// ```
///
/// So it is the statement before [`SEQUENCE`]'s `nativeSetPreferencesFile` row, the next
/// scripted downcall in `Q`'s bytecode, and the value is `NativeHelper.a`: the
/// `MainGameActivity` itself.
///
/// **`registerReceiver` is not modelled, and nothing is lost by that.** The receiver's only
/// action is `FMOD$PluginBroadcastReceiver.onReceive` -> the native
/// `OutputAAudioHeadphonesChanged`, on an `android.intent.action.HEADSET_PLUG` broadcast; this
/// host delivers no broadcasts, so a registration nothing will ever call changes nothing the
/// engine can observe.
///
/// `fi.e.E` calls `FMOD.init` again (`0x003f`, just before `nativeGameGlobalInit`) with its own
/// `Context` argument. Not repeated here: by then `gContext` is already set, `checkInit` tests
/// only for `null`, and which `Context` `E` is handed depends on which of its three callers ran
/// (`ActivityNativeMain.P2`, `fi.a.F0`, `fi.e.r`), which this script does not decide.
///
/// **The order within step 11 is the script's, not `Q`'s**, and this statement does not change
/// it: [`SEQUENCE`] runs `nativeSetExternalDirectory`, then `nativeSetPreferencesFile`, then
/// `nativeSetAppPreviousExitReasons`, where `Q` runs them in the reverse order. None of the three
/// reads `gContext`, so where among them the assignment lands is not observable to the engine.
pub const FMOD_INIT: JavaStatement = JavaStatement {
    site: "com/roblox/client/startup/NativeHelper.Q @0x0023: \
           invoke-static FMOD.init(NativeHelper.a) -> org/fmod/FMOD.init @0x0000: sput-object gContext",
    class: "org/fmod/FMOD",
    field: "gContext",
    descriptor: "Landroid/content/Context;",
    value: ScriptArg::Object("com/roblox/client/startup/MainGameActivity"),
};

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
/// The APK's own: `Roblox-2.739.691.apk` (was `Roblox-2.738.1397.apk` until 2026-09-24). Stated as a constant so that the one place it appears
/// is this one — three drifted duplicates of a figure have already appeared in this project.
pub const APP_VERSION: &str = "2.739.691";

/// `Build.VERSION.SDK_INT` of the Android this host presents, as the decimal string the APK sends.
///
/// **One figure in three places, stated once.** It is what an embedding answers for
/// `ro.build.version.sdk` (and so `activity->sdkVersion`), and it is what `DeviceParams.osVersion`
/// and `DeviceStaticParams.osVersion` carry: `fi.o.d()` and `fi.o.e()` both store
/// `Integer.toString(Build.VERSION.SDK_INT)` there -- the API level, not the release name.
/// MEASURED when the two disagreed (`osVersion` said `"13"`): the engine `strtol`s that string
/// (`0x02593f68`), compares it with its minimum API level for Vulkan (`0x0258eaa8`), and gave up
/// on Vulkan -- `Mode 6 failed: Android version is too old to activate Vulkan` -- for EGL.
/// 33 is Android 13.
pub const ANDROID_SDK_INT: &str = "33";

/// [`ANDROID_SDK_INT`] as the number `Build.VERSION.SDK_INT` is, for a Java method whose whole
/// body compares against it (`FMOD.supportsAAudio`). Parsed from the one string at compile time,
/// so the figure is still stated once and the two cannot disagree.
pub const ANDROID_SDK_LEVEL: i32 = decimal(ANDROID_SDK_INT);

/// The number a decimal string spells, at compile time. A character that is not a digit is a
/// **build failure**, not a zero: an SDK level of 0 would read as "older than every API" and
/// quietly flip every comparison made against it.
const fn decimal(text: &str) -> i32 {
    let bytes = text.as_bytes();
    assert!(!bytes.is_empty(), "the SDK level is empty");
    let mut value = 0i32;
    let mut at = 0;
    while at < bytes.len() {
        assert!(bytes[at].is_ascii_digit(), "the SDK level is not a decimal number");
        value = value * 10 + (bytes[at] - b'0') as i32;
        at += 1;
    }
    value
}

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

/// §8 steps 7-11, in order. Every edge is VERIFIED from dex bytecode (Section J). Step 12 is
/// [`ENGINE_SETTINGS`], which a device sends after step 13.
pub static SEQUENCE: &[Downcall] = &[
    // ---- step 7: RobloxApplication.onCreate ----------------------------------------------
    Downcall {
        step: 7,
        caller: "com/roblox/client/RobloxApplication.onCreate",
        class: "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        java_before: &[],
        args: &[ScriptArg::Object("android/app/Application")],
    },
    Downcall {
        step: 7,
        caller: "com/roblox/client/RobloxApplication.onCreate",
        class: "com/roblox/universalapp/linking/JNIWebLoginProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        java_before: &[],
        args: &[ScriptArg::Object("android/app/Application")],
    },
    // ---- step 8: ActivitySplash.onCreate --------------------------------------------------
    Downcall {
        step: 8,
        caller: "com/roblox/client/startup/ActivitySplash.onCreate",
        class: "com/roblox/engine/jni/NativeReportingInterface",
        member: "initAppShellReporter",
        descriptor: "()V",
        java_before: &[],
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
        java_before: &[],
        args: &[ScriptArg::Text("/data/data/com.roblox.client/cache")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.T0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetFilesDirectory",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("/data/data/com.roblox.client/files")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeInitFastLog",
        descriptor: "()V",
        java_before: &[],
        args: &[],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetRobloxVersion",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text(APP_VERSION)],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetRobloxChannel",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetBaseUrl",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("https://www.roblox.com"), ScriptArg::Text("roblox.com")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetExceptionReasonFilename",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("/data/data/com.roblox.client/files/exitReason")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.X0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetPlatformHeadersWithIdfa",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        java_before: &[],
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
        java_before: &[],
        args: &[ScriptArg::Text("0")],
    },
    // **The app's cookies, handed to the engine** -- `bh.x0.W0` calls `S0` at `0x0061`, right
    // after `nativeSetUserId`: `nativeSetMultipleCookies(g(), fl.j.b(g()) ?: "")`, `g()` being
    // `"https://" + host`, the same URL `nativeSetBaseUrl` is given, and `fl.j.b` the app's cookie
    // store. Missing until 2026-09-24, and it is why a sign-in never survived a restart. See
    // `super::cookies`.
    Downcall {
        step: 9,
        caller: "bh/x0.W0 -> bh/x0.S0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetMultipleCookies",
        descriptor: "(Ljava/lang/String;Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("https://www.roblox.com"), ScriptArg::CookiesFor("https://www.roblox.com")],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeOverrideChannelPlatformName",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text(CHANNEL_PLATFORM_NAME)],
    },
    Downcall {
        step: 9,
        caller: "bh/x0.W0",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeOverrideChannelPlatformName2",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text(CHANNEL_PLATFORM_NAME)],
    },
    // ---- step 10: MainGameActivity.b2 / .a2 -----------------------------------------------
    Downcall {
        step: 10,
        caller: "com/roblox/client/startup/MainGameActivity.b2",
        class: "com/roblox/client/startup/MainGameActivity",
        member: "nativeSetAssetPath",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        // **A directory, not the apk file.** MEASURED: passing the apk path made the engine
        // throw `'/data/app/com.roblox.client/base.apk' is not a directory`.
        //
        // **And which directory is DECODED, not chosen** -- the value this used to hold,
        // `/data/app/com.roblox.client`, was a guess that satisfied "is a directory" and nothing
        // else. `MainGameActivity.K2` passes `vk.b.n()`, the Java side's `unpackAssets`: it
        // takes `Context.getDir("assets")` -- `/data/data/<package>/app_assets` -- creates
        // `ExtraContent`, `android` and `content` under it (`vk.b$b`), and hands over
        // `app_assets/content` (`vk.b$g`, `resolve("content").toRealPath()`). The engine then
        // derives its extra-content folder from it and sets one only if `ExtraContent/` exists
        // (`0x236fbbc`-`0x236fc4c`) -- MEASURED with the guess: `setExtraAssetFolder ` empty,
        // and `rbxasset://places/Mobile.rbxl`, which lives in `ExtraContent/places`, beyond reach.
        // A device passes the `toRealPath` spelling, `/data/user/0/...`, of this same
        // directory; this root has no such link and nothing measured compares the two.
        args: &[ScriptArg::Text(ASSET_PATH)],
    },
    Downcall {
        step: 10,
        caller: "com/roblox/client/startup/MainGameActivity.a2",
        class: "com/roblox/client/startup/MainGameActivity",
        member: "nativePreloadFlagOverrides",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("")],
    },
    // ---- step 11: device info and the directories -----------------------------------------
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/MainGameActivity.E2",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetDeviceInfo",
        descriptor: "(Lcom/roblox/engine/jni/model/DeviceParams;)V",
        java_before: &[],
        args: &[ScriptArg::Object("com/roblox/engine/jni/model/DeviceParams")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetExternalDirectory",
        descriptor: "(Ljava/lang/String;)V",
        java_before: &[],
        args: &[ScriptArg::Text("/storage/emulated/0/Android/data/com.roblox.client")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeSettingsInterface",
        member: "nativeSetPreferencesFile",
        descriptor: "(Ljava/lang/String;)V",
        // `Q` calls `FMOD.init(this.a)` at `0x0023`, before this downcall at `0x003a`: the
        // statement that makes `FMOD.checkInit()` true. See `FMOD_INIT`.
        java_before: &[FMOD_INIT],
        args: &[ScriptArg::Text("/data/data/com.roblox.client/shared_prefs/prefs.xml")],
    },
    // **Where the engine's cookies go** -- `Q` calls `jk.k0.w(Context)` at `0x0046`, which first
    // touches `CookieProtocol.a()`: its class initialiser constructs the `CookieProtocol`, whose
    // constructor hands a new `CookieProtocol$OnSetCookieHandlerImpl` to `tm.b.a` -> this native.
    // The one INSTANCE native in the sequence: its `thiz` (the `JNICookieProtocol` singleton) is
    // passed as the class reference, which is safe because the body (`0x230a9f4`) never reads
    // `x1` -- it keeps only the handler, `x2`. Missing until 2026-09-24: no handler, so every
    // cookie the engine set went nowhere. See `super::cookies`.
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q -> jk/k0.w -> CookieProtocol.<init> -> tm/b.a",
        class: "com/roblox/universalapp/cookie/JNICookieProtocol",
        member: "updateOnSetCookieHandler",
        descriptor: "(Lcom/roblox/universalapp/cookie/JNICookieProtocol$OnSetCookieHandler;)V",
        java_before: &[],
        args: &[ScriptArg::Object("com/roblox/universalapp/cookie/CookieProtocol$OnSetCookieHandlerImpl")],
    },
    Downcall {
        step: 11,
        caller: "com/roblox/client/startup/NativeHelper.Q",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativeSetAppPreviousExitReasons",
        descriptor: "(Ljava/util/List;)V",
        java_before: &[],
        // `jk.l2.a`: the exits the system recorded, as `ApplicationExitInfoCpp`s. The embedding
        // records them (`Jni::set_previous_exits`); none is a fresh install's empty list.
        args: &[ScriptArg::PreviousExitReasons],
    },
];

/// §8 step 12, the app-bridge init parameters -- which a device sends **after** the engine
/// exists, so it is not in [`SEQUENCE`].
///
/// # The engine drops them when they arrive first, and says so
///
/// MEASURED, with this downcall at the end of [`SEQUENCE`] (before step 13 creates anything):
///
/// ```text
/// [FLog::NativeMain] [android_main] nativeAppBridgeSetInitParams: ERROR: nativeEngine is not created!
/// ```
///
/// `Java_..._nativeAppBridgeSetInitParams` (`0x02bcc814`) builds the settings and then, at
/// `0x02bcccc8`, loads the `NativeEngine*` global at `0x0683d888`: null logs the line above and
/// discards them; non-null calls `NativeEngine::setEngineSettings` (`0x02bcddac`). That is the
/// **only** caller of `nativeActivity_onEngineSettingsReceived` (`0x02bd1c38`), which sets the
/// `NativeDataModelManager` byte at `+0x288`. `continueAfterFlagsLoaded_` (`0x02bd3b58`) sets its
/// neighbour at `+0x289`; whichever of the two lands second moves the state to 3, and the main
/// loop's step (`0x02bd1cf0`) calls `initEngine_` only in state 3. Dropped here, the settings
/// never arrive, `initEngine_` never runs, and the Lua app is never initialised -- MEASURED as a
/// null `SingleSurfaceAppImpl + 0x28` (set only by `initializeWithAppStarter`) read when a later
/// start reached `userDidLogin`.
///
/// # Why after, from the dex
///
/// The caller is `MainGameActivity.E2` ("setInitParamsForEngine"), reached from `B2` once the
/// assets are unpacked -- work `onCreate` starts only after `super.onCreate`, which is
/// `GameActivity.onCreate` and so `initializeNativeCode`, whose thread creates the engine. `E2`
/// runs once (an `AtomicBoolean` guards it). The host sends this after the engine has answered
/// lifecycle rows, which is the earliest point it can know the engine exists.
pub static ENGINE_SETTINGS: &[Downcall] = &[Downcall {
    step: 12,
    caller: "com/roblox/client/startup/MainGameActivity$Companion.e",
    class: "com/roblox/client/startup/MainGameActivity",
    member: "nativeAppBridgeSetInitParams",
    descriptor: "(Lcom/roblox/engine/jni/autovalue/InitParams;)V",
    java_before: &[],
    args: &[ScriptArg::Object("com/roblox/engine/jni/autovalue/InitParams")],
}];

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

/// `nativeSetAssetPath`'s argument: the Java side's unpacked-assets directory. See its row in
/// [`SEQUENCE`] for how it was decoded; an embedding creates it and its two siblings, as the Java
/// side does ([`ASSET_DIRECTORIES`]).
pub const ASSET_PATH: &str = "/data/data/com.roblox.client/app_assets/content";

/// The three directories `vk.b$b` creates under `Context.getDir("assets")`, in its order. Their
/// existence is load-bearing: the engine sets its extra-content folder only if `ExtraContent`
/// is there.
pub const ASSET_DIRECTORIES: &[&str] = &[
    "/data/data/com.roblox.client/app_assets/ExtraContent",
    "/data/data/com.roblox.client/app_assets/android",
    "/data/data/com.roblox.client/app_assets/content",
];

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
        java_before: &[],
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
        java_before: &[],
        args: &[ScriptArg::Object("java/util/List")],
    },
    // ---- row 22: global init, then the app itself -------------------------------------------
    Downcall {
        step: 22,
        caller: "fi/e.E",
        class: "com/roblox/engine/jni/NativeGLInterface",
        member: "nativeGameGlobalInit",
        descriptor: "()V",
        java_before: &[],
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
    // `RobloxApplication.onCreate`'s `JNIAAssetManagerSetup.initNative(AssetManager)`: a static
    // native on a class nothing looks members up on, like the rest of this list.
    "com/roblox/client/JNIAAssetManagerSetup",
    "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
    "com/roblox/universalapp/linking/JNIWebLoginProtocol",
    "com/roblox/engine/jni/NativeReportingInterface",
    "com/roblox/engine/jni/NativeSettingsInterface",
    "com/roblox/engine/jni/NativeGLInterface",
    // `RobloxApplication.onCreate` registers one with `ProcessLifecycleOwner`; see
    // [`process_lifecycle`].
    "com/roblox/universalapp/applifecyclenativeadapter/JNIAppLifecycleNativeAdapter",
    // `NativeHelper.Q`'s cookie handler registration: an instance native whose `thiz` the engine
    // never reads (see its row), so the class stands in for the singleton.
    "com/roblox/universalapp/cookie/JNICookieProtocol",
];

/// The guest value one scripted argument becomes, made the way the Java side makes it.
///
/// # Errors
///
/// Whatever building the object refuses for.
pub fn guest_argument(jni: &Jni, argument: &ScriptArg) -> AbiResult<GuestArg> {
    Ok(match argument {
        ScriptArg::Text(text) => GuestArg::Int(jni.new_string(text)?),
        ScriptArg::Object(class) => GuestArg::Int(jni.new_object(class)?),
        ScriptArg::Null => GuestArg::Int(0),
        ScriptArg::Long(value) => GuestArg::Int(*value as u64),
        ScriptArg::PreviousExitReasons => GuestArg::Int(jni.previous_exit_reasons()?),
        ScriptArg::CookiesFor(url) => GuestArg::Int(jni.new_string(&jni.cookie_header(url))?),
    })
}

/// A process lifecycle event, as androidx's `ProcessLifecycleOwner` dispatches it to the
/// observers registered on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEvent {
    /// `ON_RESUME`: dispatched once the first activity has resumed.
    Resume,
    /// `ON_PAUSE`: dispatched `ProcessLifecycleOwner.TIMEOUT_MS` (700 ms) after the last activity
    /// paused.
    Pause,
    /// `ON_STOP`: dispatched after that pause once the last activity has stopped.
    Stop,
}

impl ProcessEvent {
    /// The static native `JNIAppLifecycleNativeAdapter.g` calls for the event -- DECODED from
    /// `classes2.dex`: its switch maps `ON_RESUME` to `setActive`, `ON_PAUSE` to `setInactive` and
    /// `ON_STOP` to `setHidden`, and ignores the rest.
    #[must_use]
    pub fn native(self) -> &'static str {
        match self {
            ProcessEvent::Resume => "setActive",
            ProcessEvent::Pause => "setInactive",
            ProcessEvent::Stop => "setHidden",
        }
    }
}

/// **Deliver a process lifecycle event to the engine**, as the observer the app registers does.
///
/// `RobloxApplication.onCreate`, on the GameActivity path (`"GameActivity = ON. Return after
/// loading native libs!"`), adds a `JNIAppLifecycleNativeAdapter` to
/// `ProcessLifecycleOwner.get().getLifecycle()`; its `onStateChanged` (`g`) calls one of three
/// static natives, exported by `libroblox.so`, with no arguments. MEASURED why the host must do
/// this: a run that closed the app without it left the engine's session record saying the app never
/// left the foreground, and the next launch judged that session a crash.
///
/// # Errors
///
/// [`AbiError::JniRefused`] naming the export when `resolve` does not know it, and whatever the
/// call itself fails with.
pub fn process_lifecycle(
    jni: &Arc<Jni>,
    boundary: &Arc<Boundary>,
    cpu: &mut dyn GuestCpu,
    resolve: &dyn Fn(&str) -> Option<GuestAddr>,
    event: ProcessEvent,
) -> AbiResult<()> {
    const CLASS: &str = "com/roblox/universalapp/applifecyclenativeadapter/JNIAppLifecycleNativeAdapter";
    let symbol = format!(
        "Java_com_roblox_universalapp_applifecyclenativeadapter_JNIAppLifecycleNativeAdapter_{}",
        event.native()
    );
    let Some(target) = resolve(&symbol) else {
        return Err(AbiError::JniRefused {
            function: symbol,
            address: 0,
            detail: "the app's process lifecycle observer calls this export, and nothing resolved it"
                .to_string(),
        });
    };
    let class = jni.class_reference(CLASS)?;
    let args = [GuestArg::Pointer(jni.env_for(0)), GuestArg::Int(class)];
    boundary.call_guest(cpu, &format!("ProcessLifecycleOwner {event:?} ({CLASS})"), target, &args, PER_DOWNCALL)?;
    Ok(())
}

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
        // **The Java statements first, whether or not the export resolves**: on a device they
        // run as the calling method reaches them, and a native that fails to link fails at its
        // own call, after them. A statement that cannot be performed is this step's failure and
        // the downcall is not made -- the Java method would have thrown before reaching it.
        if let Err(error) = perform(jni, step.java_before) {
            outcomes.push(StepOutcome {
                step: step.step,
                target: resolve(&symbol),
                symbol,
                result: Err(error),
                returned: None,
                last_segment_instructions: 0,
            });
            continue;
        }
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
            args.push(guest_argument(jni, argument)?);
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

/// Perform Java statements, in order, against `jni`.
///
/// What [`run`] does with each row's [`Downcall::java_before`]; public so that an embedding that
/// drives its own sequence performs the same statements the same way.
///
/// # Errors
///
/// [`AbiError::JniRefused`] naming the statement's site when a value cannot be built or the store
/// is refused -- an undeclared class or field, a field not declared
/// [`Answer::Assigned`](super::classes::Answer::Assigned), a value its type does not admit.
pub fn perform(jni: &Jni, statements: &[JavaStatement]) -> AbiResult<()> {
    for statement in statements {
        let named = |error: AbiError| AbiError::JniRefused {
            function: "script::perform".to_string(),
            address: 0,
            detail: format!("the Java statement at {} could not be performed: {error}", statement.site),
        };
        let value = match statement.value {
            ScriptArg::Object(class) => jni.new_object(class).map_err(named)?,
            ScriptArg::Null => 0,
            // A statement stores an instance or `null` -- `JavaStatement::value` says so -- and
            // the store checks instance types against the field's. A `jlong` is no reference at
            // all, and no field the script assigns holds a string: refused, not converted.
            ScriptArg::Text(_) | ScriptArg::Long(_) | ScriptArg::PreviousExitReasons | ScriptArg::CookiesFor(_) => {
                return Err(named(AbiError::JniRefused {
                    function: "script::perform".to_string(),
                    address: 0,
                    detail: format!(
                        "{:?} is not an instance or null, which is what a Java statement stores \
                         into `{}.{}`",
                        statement.value, statement.class, statement.field
                    ),
                }))
            }
        };
        jni.put_static_object(statement.class, statement.field, statement.descriptor, value)
            .map_err(named)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every symbol the script calls is the **short** mangling of its `(class, member)`, which is
    /// what Section G tags each of them. A name that needed an escape would be caught here.
    #[test]
    fn the_scripted_symbols_are_the_short_mangling_of_their_members() {
        for step in SEQUENCE.iter().chain(ENGINE_SETTINGS) {
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
        assert_eq!(count(9), 12, "§8 step 9: the 11 `(String…)V` downcalls and `nativeSetMultipleCookies`");
        assert_eq!(count(10), 2, "nativeSetAssetPath and nativePreloadFlagOverrides");
        assert_eq!(count(11), 5, "nativeSetDeviceInfo, External, Preferences, the cookie handler, ExitReasons");
        // Step 12 is not here: the engine drops it before step 13 (see `ENGINE_SETTINGS`).
        assert_eq!(count(12), 0, "nativeAppBridgeSetInitParams waits for the engine");
        assert_eq!(SEQUENCE.len(), 22);
        assert_eq!(ENGINE_SETTINGS.len(), 1);
        assert_eq!(ENGINE_SETTINGS[0].member, "nativeAppBridgeSetInitParams");
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
            // Strings the Java side builds: literals, and the one it reads out of its cookie store.
            for argument in step.args {
                assert!(matches!(argument, ScriptArg::Text(_) | ScriptArg::CookiesFor(_)), "{}", step.member);
            }
        }
    }

    /// Every class a step names is either declared by default or in [`SCRIPT_CLASSES`]. A step
    /// whose class is in neither would fail on its `jclass` rather than on the engine.
    #[test]
    fn every_class_the_script_names_is_declarable() {
        let declared: Vec<&str> =
            super::super::classes::DECLARED.iter().map(|spec| spec.name).collect();
        for step in SEQUENCE.iter().chain(ENGINE_SETTINGS) {
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
            for statement in step.java_before {
                assert!(declared.contains(&statement.class), "{}", statement.site);
                if let ScriptArg::Object(class) = statement.value {
                    assert!(declared.contains(&class), "{}", statement.site);
                }
            }
        }
    }

    /// `FMOD.checkInit()`, called the way the engine calls it.
    fn check_init(jni: &Jni) -> bool {
        let mut state = jni.state();
        let class = state.registry.find("org/fmod/FMOD").expect("declared");
        let method = state.registry.method(class, "checkInit", "()Z", true).expect("declared");
        let member = state.registry.member(method).expect("a member").clone();
        match super::super::env::evaluate(
            &mut state,
            "CallStaticBooleanMethodV",
            0,
            class,
            &member,
            None,
            &[],
        ) {
            Ok(super::super::values::Value::Boolean(set)) => set,
            other => panic!("checkInit answered {other:?}"),
        }
    }

    /// The SDK level is the string, parsed -- not a second figure that could drift from it.
    #[test]
    fn the_sdk_level_is_the_sdk_string_as_a_number() {
        assert_eq!(ANDROID_SDK_LEVEL, 33);
        assert_eq!(ANDROID_SDK_LEVEL, ANDROID_SDK_INT.parse::<i32>().expect("a decimal"));
        assert_eq!(decimal("27"), 27);
        assert_eq!(decimal("0"), 0);
    }

    /// **`FMOD.init` is performed by exactly one row: the one whose Java method performs it.**
    ///
    /// `NativeHelper.Q` calls it at `0x0023` with `NativeHelper.a`, the `MainGameActivity`, and
    /// the next scripted downcall in `Q` is `nativeSetPreferencesFile`. A table that dropped the
    /// statement would leave `checkInit` false for the whole run -- the engine would then skip
    /// FMOD's asset reader and take the no-Context branches of its output choice -- and one that
    /// attached it to an earlier step would claim `FMOD.init` ran before the Java that runs it.
    #[test]
    fn fmod_init_is_performed_by_the_row_whose_java_performs_it_and_by_no_other() {
        let rows: Vec<&Downcall> = SEQUENCE
            .iter()
            .chain(ENGINE_SETTINGS)
            .chain(FLAGS_AND_START)
            .filter(|row| !row.java_before.is_empty())
            .collect();
        assert_eq!(rows.len(), 1, "one row performs Java statements: {rows:?}");
        let row = rows[0];
        assert_eq!(
            (row.step, row.caller, row.member),
            (11, "com/roblox/client/startup/NativeHelper.Q", "nativeSetPreferencesFile")
        );
        assert_eq!(row.java_before, &[FMOD_INIT]);
        assert_eq!(
            (FMOD_INIT.class, FMOD_INIT.field, FMOD_INIT.descriptor),
            ("org/fmod/FMOD", "gContext", "Landroid/content/Context;")
        );
        assert_eq!(FMOD_INIT.value, ScriptArg::Object("com/roblox/client/startup/MainGameActivity"));
    }

    /// **[`run`] performs a row's Java statements before its downcall, and only at that row** --
    /// driven through `run` itself, one row at a time as the gate drives it, with an export
    /// table that knows nothing, so no guest code runs and what is observed is the statement.
    ///
    /// `checkInit` is `false` through every row before the one that performs `FMOD.init`, and
    /// `true` from it on. The statement is performed even though the export did not resolve:
    /// on a device `FMOD.init` runs before the native is even looked up.
    #[test]
    fn run_performs_fmod_init_at_its_row_and_not_before() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(Arc::clone(&space)).expect("a JNI instance");
        declare_script_classes(&jni);
        let boundary = crate::boundary::BoundaryBuilder::new(Arc::clone(&space), 1, 4096)
            .expect("a thunk region")
            .finish();
        let backend = omni_cpu::dynarmic::DynarmicBackend::new(
            Arc::clone(&space),
            omni_cpu::dynarmic::DynarmicOptions::default(),
        )
        .expect("a backend");
        let mut cpu = omni_cpu::GuestCpuBackend::create_guest_thread(&backend).expect("a thread");

        let at = SEQUENCE
            .iter()
            .position(|row| row.java_before.contains(&FMOD_INIT))
            .expect("a row performs FMOD.init");
        for (index, row) in SEQUENCE.iter().enumerate() {
            let outcomes =
                run(&jni, &boundary, cpu.as_mut(), &|_| None, std::slice::from_ref(row), 0)
                    .expect("the arguments build");
            assert_eq!(outcomes.len(), 1);
            assert!(outcomes[0].target.is_none() && !outcomes[0].ok(), "{:?}", outcomes[0]);
            assert_eq!(
                check_init(&jni),
                index >= at,
                "after row {index} (`{}`), FMOD.init is at row {at}",
                row.member
            );
        }
    }

    /// **`nativeSetMultipleCookies` is handed what the app's cookie store answers for its URL** --
    /// the startup half of `super::super::cookies` -- and `""` when it holds nothing, as
    /// `bh.x0.S0` passes then.
    #[test]
    fn the_startup_cookie_argument_is_the_stores_answer_for_its_url() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(Arc::clone(&space)).expect("a JNI instance");
        let row = SEQUENCE.iter().find(|row| row.member == "nativeSetMultipleCookies").expect("the row");
        let [ScriptArg::Text(url), cookies @ ScriptArg::CookiesFor(for_url)] = row.args else {
            panic!("(url, the store's cookies for it): {:?}", row.args);
        };
        assert_eq!(url, for_url, "the same URL twice, as `S0` passes `g()` twice");
        let text = |argument: GuestArg| match argument {
            GuestArg::Int(handle) => jni.string_of(handle).expect("a handle").expect("a string"),
            other => panic!("{other:?}"),
        };
        assert_eq!(text(guest_argument(&jni, cookies).expect("built")), "", "an empty store");
        let dir = std::env::temp_dir().join(format!("omni-script-cookies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("omnidroid-cookies");
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(&file, "omnidroid-cookies v1\nA\t1\troblox.com\t0\t/\t-\t1\t1\n").expect("a store");
        jni.set_cookie_store(&file).expect("the store");
        assert_eq!(text(guest_argument(&jni, cookies).expect("built")), "A=1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An event whose export nothing resolves is refused, naming the export: the engine would
    /// otherwise never hear it, and nothing downstream would say so.
    #[test]
    fn a_process_event_with_no_export_is_refused_by_name() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(Arc::clone(&space)).expect("a JNI instance");
        declare_script_classes(&jni);
        let boundary = crate::boundary::BoundaryBuilder::new(Arc::clone(&space), 1, 4096)
            .expect("a thunk region")
            .finish();
        let backend = omni_cpu::dynarmic::DynarmicBackend::new(
            Arc::clone(&space),
            omni_cpu::dynarmic::DynarmicOptions::default(),
        )
        .expect("a backend");
        let mut cpu = omni_cpu::GuestCpuBackend::create_guest_thread(&backend).expect("a thread");
        for event in [ProcessEvent::Resume, ProcessEvent::Pause, ProcessEvent::Stop] {
            match process_lifecycle(&jni, &boundary, cpu.as_mut(), &|_| None, event) {
                Err(AbiError::JniRefused { function, .. }) => assert_eq!(
                    function,
                    format!(
                        "Java_com_roblox_universalapp_applifecyclenativeadapter_JNIAppLifecycleNativeAdapter_{}",
                        event.native()
                    )
                ),
                other => panic!("{event:?} answered {other:?}"),
            }
        }
    }

    /// A statement that cannot be performed is its row's failure, and the downcall is not made:
    /// the Java method would have thrown before reaching it.
    #[test]
    fn a_statement_that_cannot_be_performed_fails_its_row() {
        let space = Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let wrong = JavaStatement { value: ScriptArg::Object("java/util/List"), ..FMOD_INIT };
        let error = perform(&jni, &[wrong]).expect_err("a List is not a Context");
        match error {
            AbiError::JniRefused { detail, .. } => {
                assert!(detail.contains("NativeHelper.Q"), "the site is named: {detail}")
            }
            other => panic!("{other:?}"),
        }
        let long = JavaStatement { value: ScriptArg::Long(7), ..FMOD_INIT };
        assert!(perform(&jni, &[long]).is_err(), "a jlong is not a reference");
        assert!(!check_init(&jni), "nothing was stored");
        perform(&jni, &[FMOD_INIT]).expect("the real statement");
        assert!(check_init(&jni));
    }
}
