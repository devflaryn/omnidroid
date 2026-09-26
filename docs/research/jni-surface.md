# JNI surface of `libroblox.so`

Target: `lib/arm64-v8a/libroblox.so` of `Roblox-2.738.1397.apk` (109,193,800 B; byte-identical in
the stock APK and the earlier modified fixture). Raw lists: `jni-surface-lists.txt` (sections
A1-M). APK facts: `apk-analysis.md`. Analysed 2026-09-18; dex counts re-checked on the stock APK
2026-09-26.

**Scope note.** Stock Roblox, AGDK and AndroidX only. The earlier fixture's injected payload
(`classes4.dex`, `com.roblox.gloop.*`, its trojanised `libzstd-jni`) was kept out of every count;
the stock APK has none of it. Tools that still skip `classes4.dex` (`gen_dex_surface.py`,
`tools/dexdis.py`) find nothing to skip. VERIFIED = read from the bytes at a stated address.

## 0. Headline numbers

| Question | Answer |
|---|---:|
| `JNINativeInterface` slots dereferenced | 59 of 233 (170 never, 4 reserved) |
| JNI call sites | 943 (the sum of Section A1) |
| `JavaVM` slots used | 2 (`GetEnv`, `AttachCurrentThread`) over 5 sites |
| Class-name / descriptor literals | 128 / 135, complete |
| Java members looked up | 409 (296 methods, 113 fields) in 104 classes |
| of which Djinni protocol bridges (lazy) | 109 members in 26 classes |
| Dex native methods (stock) | 700: 662 exported (657 short + 5 long names), 38 `RegisterNatives` |
| Natives on the startup path | about 34 (§4.2) |

A dex interpreter is not required: nothing forces dex execution (§6). The Java side initiates a
large part of startup, so the host drives the engine with a scripted downcall sequence (§8); that
is orchestration, not interpretation. The first blocker was not JNI: 1,282 `MRS Xt, TPIDR_EL0`,
1,276 of them followed by a read of `[Xt, #0x28]` (bionic's stack guard), so `TPIDR_EL0` must hold
a bionic TLS block on every guest thread (D13).

## 1. Method

1. Function bounds: 245,117 `.eh_frame_hdr` FDEs. 2. Strings: 135 descriptors and 128 class
names in `.rodata`; no other section has any, so both lists are complete. 3. `ADRP`+`ADD`
cross-references. 4. `JNIEnv` taint from the 539 `Java_*` exports, `JNI_OnLoad` and the 26
registered functions, by straight-line symbolic execution (capstone 5.0.7); a JNI call is
`ldr Xb,[env]; ldr Xt,[Xb,#imm]; blr Xt` with a tainted base, which excludes look-alike C++
virtual calls.

5. Offset to function: `jni.h` declaration order, 233 entries, table `0x748` bytes, offset =
index x 8 (`FindClass` 0x30, `GetObjectClass` 0xf8, `GetMethodID` 0x108, `GetFieldID` 0x2f0,
`GetStaticMethodID` 0x388, `GetStaticFieldID` 0x480, `NewStringUTF` 0x538, `RegisterNatives`
0x6b8, `GetJavaVM` 0x6d8, `ExceptionCheck` 0x720). The mapping validated itself: every site with
a literal string argument lands on one of those six offsets with the matching argument shape. An
earlier hand decoder reported 9 hits on the reserved slots; capstone reported none, which exposed
them as false positives.

6. Class attribution, tagged per row in Section D (dataflow from `FindClass`, unique dex match,
inspection). 7. Dex cross-check of attributed lookups. 8. Djinni helpers (`jniFindClass`,
`jniGetMethodID`, `jniGetFieldID`) fetch the env themselves: 151 more lookups. 9. Dex `invoke-*`
scan for the Java callers of every native (Sections I-J). 10. PLT names from `.rela.plt`.

### 1.1 Limits

Straight-line, not CFG-based: misses are likelier than false positives. 135 of 313 direct lookups
had no class by dataflow; 21 remain `!unresolved`/`!ambiguous` in Section D. 409 is a lower bound.

## 2. Native to Java

### 2.1 JNI functions used

59 slots (Section A1). Every `Call*Method` goes through the `...V` (`va_list`) slot, one call
site each, via the `jni.h` inline wrappers; there are no `...A` or plain-varargs sites. Never
used (Section A3) include `DefineClass`, `GetSuperclass`, `IsInstanceOf`, `AllocObject`,
`PushLocalFrame`, `MonitorEnter`, `GetVersion`, every `FromReflected*`/`ToReflected*` and every
`Set*Field`/`SetStatic*Field`: the engine never writes a Java field.

### 2.2 JavaVM contract

`GetEnv` (0x30) at `0x2174de0`, `0x21750b0`, `0x21e3bc0`, `0x21e3f18`, with version `0x00010006`
(`JNI_VERSION_1_6`). `AttachCurrentThread` (0x20) at `0x2174e1c`, `0x21750ec`, only after
`JNI_EDETACHED`, with a thread name in `JavaVMAttachArgs`. `DetachCurrentThread` and
`DestroyJavaVM` are never called. `JNI_OnLoad` is at `0x2173ff4` and must see `0x00010006`
accepted.

### 2.3 Names

64 direct `FindClass` sites with a literal, 58 more through Djinni helpers. Section B lists the
128 class literals; 59 are `com/roblox/protocols/*`.

## 3. Class requirements

Section D has every row: 104 classes (JDK 16, Android 9, AndroidX 2, AGDK 3, app 70, unresolved 4).

### 3.1 Tiers

**Tier 0: startup aborts without these.**

| Class | Members | Why |
|---|---|---|
| `com/google/androidgamesdk/GameActivity` | `finish()V`, `setWindowFlags(II)V`, `getWindowInsets(I)`, `getWaterfallInsets()`, `setImeEditorInfoFields(III)V` | resolved by `0x285a614` before `initializeNativeCode`'s body; `CHECK_NOT_NULL` aborts |
| `androidx/core/graphics/Insets` | `left`, `top`, `right`, `bottom` | read right after `getWindowInsets` |
| `androidx/core/view/WindowInsetsCompat$Type` | 9 static `()I` | inset mask |
| `android/content/res/Configuration` | 18 fields + `getLocales()` | `initializeNativeCode`'s configuration argument, read at `0x285c84c` |
| `java/lang/String` | `getBytes(String)` | plus `NewStringUTF`/`GetStringUTFChars` |
| `android/app/ActivityThread` | `currentActivityThread()`, `currentApplication()`, `getApplication()` | the engine fetches its own `Context` |
| `java/lang/ClassLoader` | `loadClass`, `findClass`, `getClassLoader()` | Djinni's cached app class loader, not code loading |

`Configuration`'s fields: `mcc mnc orientation touchscreen keyboard keyboardHidden
hardKeyboardHidden navigation navigationHidden screenLayout uiMode screenWidthDp screenHeightDp
smallestScreenWidthDp densityDpi colorMode fontScale fontWeightAdjustment`. This file once called
all 18 `int`; `fontScale` is a `float` and Section D never resolved the descriptors, so
`classes.rs` declares it `F`.

**Tier 1: engine bootstrap and a surface.** `MainGameActivity` (`getNativeHelper`,
`bootstrapTheApp`, ...), `NativeHelper` (23 `gameActivity_*` callbacks), `NativeGLJavaInterface`
(27), `NativeUserJavaInterface`, `NativeLocaleJavaInterface`, `SessionReporterJavaInterface`,
`ClientLocalFlags`, `NativeFlagsInitResult`, `NativeTextBoxInfo`,
`LoggingProtocol.getProcessTimestamp()J` (looked up inside `JNI_OnLoad`), `DisplayMetrics`/
`Resources`/`Context`, `LocaleList`/`Locale`, `HashMap`/`List`, boxed primitives.

**Tier 2: input and text.** `MotionEvent`, `KeyEvent`, `gametextinput/State`,
`gametextinput/InputConnection`. The glue reads Java event objects through JNI getters, which is
why no `AMotionEvent_*` is imported (`apk-analysis.md` §4.4).

**Tier 3: lazy.** The 26 Djinni `com/roblox/protocols/*/generated/*` bridges and feature classes
(IAP, video, battery, FMOD, WebRTC, widgets, ...), resolved on first use.

**Tier X: absent from the dex.** `com/roblox/platform/util/DeviceUtils` and five `signalVideo*`
methods have no declaring class; the engine tolerates `NULL` there, so a miss must leave a pending
exception, not abort.

### 3.2 Injected payload

Not applicable to the stock APK (the old fixture's `com.roblox.gloop.Loader` was excluded here).

## 4. Java to native

Of the 700 dex natives, 662 are exported `Java_*` (5 only under the long, overloaded name) and 38
need `RegisterNatives`: `GameActivity` 23, `AppsFlyer2dXConversionCallback` 7 (no library, dead),
`zstd.Zstd` 3, `org.fmod.MediaCodec` 2, `NativeAppBridgeInterface`, `org.webrtc.Logging`,
`WebRtcAudioManager` 1 each. 52 `Java_*` exports have no dex native (dead exports).

### 4.1 The 24 GameActivity natives

The `JNINativeMethod[24]` array in `.data.rel.ro` at `0x062dc1c8` (pointers are APS2 `RELATIVE`
relocations) matches `GameActivity`'s 24 dex natives; Section F lists names, descriptors and
addresses. `initializeNativeCode`'s descriptor is
`(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Landroid/content/res/AssetManager;[BLandroid/content/res/Configuration;)J`.
The exported `initializeNativeCode` (`0x0285b6dc`) runs the one-time class-info init
(`bl 0x285a614`) and tail-calls the registered implementation `0x0285b750`.

### 4.2 Startup call edges

VERIFIED from dex bytecode; Section J lists each edge with its caller, and §8 rows 7-24 give the
order. Roblox's `GameActivity` fork extends `Lj/b;` (AppCompatActivity) and routes game input
through its own `vk.e` listener and `NativeInputInterface.nativePassInput*`, not AGDK's
`onTouchEventNative`.

## 5. AGDK GameActivity

### 5.1 Statically linked

The native table, its 24 functions (`0x0285b750`-`0x0285c6a0`), the AGDK assertion strings,
`android_native_app_glue` and GameTextInput are all inside `libroblox.so`; no other library has
any of it.

### 5.2 Native-side contract (Section K)

`initializeNativeCode` (`0x0285b750`):

1. `operator new(0x278)`: a 632-byte `NativeCode`; its first 0x50 bytes are `GameActivity`.
2. `__system_property_get("ro.build.version.sdk")` -> `sdkVersion` (+0x30).
3. `ALooper_forThread()` + `ALooper_acquire()` -> +0x158. NULL logs `"Unable to retrieve native
   ALooper"` and returns 0.
4. `pipe()` + `fcntl(F_SETFL, O_NONBLOCK)` x2 -> `msgread`/`msgwrite` (+0x150/+0x154).
5. `ALooper_addFd(looper, msgread, 0, ALOOPER_EVENT_INPUT, 0x285d57c, this)`.
6. `callbacks = this + 0x50` (+0x00).
7. `GetJavaVM` -> `vm` (+0x08); `env` (+0x10). Failure logs `"GameActivity GetJavaVM failed"`.
8. `NewGlobalRef(thiz)` -> `javaGameActivity` (+0x18).
9. The three path strings -> `internalDataPath` (+0x20), `externalDataPath` (+0x28), `obbPath`
   (+0x48).
10. `NewGlobalRef(jAssetMgr)` (+0x160) and `AAssetManager_fromJava` -> `assetManager` (+0x40).
11. The `Configuration` fields and `getLocales()` (helper `0x285c84c`).
12. `savedState` bytes, then `GameActivity_onCreate(activity, savedState, size)` (`0x0285e7c8`).
13. `GameTextInput_init(env, 0)` -> +0x168, and its event callback.
14. Returns `NativeCode*` as the `jlong` handle passed to the other 23 natives.

`GameActivity_onCreate` (glue): fills all 21 `GameActivityCallbacks`; allocates a 384-byte
`android_app` (`activity` +0x10, mutex +0xc8, cond +0xf0, command pipe +0x120/+0x124); creates a
detached thread on `android_app_entry`; then **waits on the cond until `app->running`**; sets
`activity->instance` (+0x38).

`android_app_entry` (`0x0285f8d8`, game thread): `AConfiguration_new` / `_fromAssetManager` /
`_getLanguage` / `_getCountry`; input buffers (motion 0x6e00 B x2, key 0x100 B x2);
`ALooper_prepare(1)`; `ALooper_addFd(msgread, LOOPER_ID_MAIN)`; `running = 1` and broadcast; then
`android_main` (`0x02bcc6a4`), which constructs an 808-byte `NativeEngine` (`0x02bcd02c`), stores it
in the global `0x0683d888` and runs `NativeEngine::GameLoop` (`0x02bcd5d0`). Neither
`GameActivity_onCreate` nor `android_main` is exported.

### 5.3 AGDK version

Not pinnable: no version file or string, and Roblox modified the source. At least
`game-activity` 2.0 (the 15-scalar `onTouchEventNative`, `setInputConnectionNative`,
`gametextinput/State`). Use Section K as the contract.

## 6. Nothing forces dex execution

All negative (Section M): `dalvik/system/*`, reflection, `Class.forName`, `java/lang/invoke`,
`DefineClass`, `FromReflected*`, Java HTTP, Java file I/O, `android/webkit`, SQLite. Networking and
files are native. The only 2 `RegisterNatives` sites are in `libroblox.so`'s own startup; the
`ClassLoader` lookups are the cached-loader pattern. What remains is native reimplementation of
Java behaviour: the `NativeHelper`/`fi.e`/`bh.x0` orchestration (§8),
`ActivityThread.currentApplication()` with `Resources`/`DisplayMetrics`, and input objects.

## 7. Injected payload

Not applicable to the stock APK. `libroblox.so`'s two 1-entry tables at `0x0667e478` and
`0x0667edd8` (`nativeCacheAudioParameters`) are stock WebRTC.

## 8. Startup contract

V = VERIFIED in the binary or dex; I = inferred. Rows 7-12 and 21-22 are the downcalls in
`crates/omni-android/src/jni/script.rs`.

| # | Step | Host must provide | Ev. |
|---:|---|---|---|
| 1 | Inflate `lib/arm64-v8a/*.so` (all DEFLATED) | extracted libraries | V |
| 2 | Map `libroblox.so`; 568,272 APS2 relocations + 534 `JUMP_SLOT` | APS2 decoder, bind-now | V |
| 3 | `DT_NEEDED` and 565 imports | bionic libc; 27 `libandroid` imports | V |
| 4 | `TPIDR_EL0` = bionic TLS block per thread, `+0x28` stack guard | per-thread TLS | V |
| 5 | Run 3,594 `DT_INIT_ARRAY` entries | `__cxa_atexit`, `malloc`, `pthread_key_*`, stack guard | V |
| 6 | `JNI_OnLoad(vm)` at `0x2173ff4`, expect `0x00010006` | `JavaVM` with `GetEnv`, `AttachCurrentThread` | V |
| 6a | VM cached at `0x07275550`; attach helper `0x2174c04`; `LoggingProtocol.getProcessTimestamp` lookup + `ExceptionCheck` | `FindClass`, `GetStaticMethodID`, `ExceptionCheck` | V |
| 6b | `0x2174c90`, `0x2174e58`, `0x2175128` resolve Tier 0/1 | §3.1 classes | V |
| 7 | `RobloxApplication`/`ActivityProtocolLaunch.onCreate`: `JNIBaseUrlProtocol`, `JNIWebLoginProtocol` | a `Context` | V |
| 8 | `ActivitySplash.onCreate`: `initAppShellReporter()` | | V |
| 9 | `bh.x0` settings: `nativeInitFastLog`, `nativeSetRobloxVersion`, `...BaseUrl`, ... (11 downcalls) | strings | V |
| 10 | `nativeSetAssetPath`, `nativePreloadFlagOverrides` | | V |
| 11 | `nativeSetDeviceInfo`, `...ExternalDirectory`, `...PreferencesFile`, cookie handler, `nativeSetAppPreviousExitReasons` | `DeviceParams`, `List` | V |
| 12 | `nativeAppBridgeSetInitParams(InitParams)`: sent after 13 (earlier, the engine logs `nativeEngine is not created!` and drops it; MEASURED) | `InitParams` | V |
| 13 | Exported `initializeNativeCode(env, thiz, 3 paths, assetMgr, savedState, config)`; Tier 0 lookups | `AssetManager`, `Configuration` | V |
| 13a | A real looper from `ALooper_forThread` (else it returns 0); `pipe`, `fcntl`, `AAssetManager_fromJava` | NDK + libc | V |
| 13b | Returns the `NativeCode*` handle, argument 1 of the other 23 natives | | V |
| 14 | Game thread spawned; step 13 blocks on `pthread_cond_wait` until `app->running` | pthreads, `AConfiguration_*`, second looper | V |
| 15 | `android_main` -> `NativeEngine(app)` (808 B) -> `GameLoop()` | | V |
| 16 | `setInputConnectionNative(handle, InputConnection)` | `InputConnection`, `State` | V |
| 17 | `onSurfaceCreatedNative`: `ANativeWindow_fromSurface` -> `NativeCode+0x140`, `callbacks[7]` -> `APP_CMD_INIT_WINDOW` | a `Surface` -> `ANativeWindow` | V |
| 18 | `onSurfaceChangedNative(handle, Surface, format, w, h)` -> `callbacks[7]`/`[8]`/`[10]` | `ANativeWindow_getWidth/getHeight/acquire/release` | V |
| 19 | `onStartNative` -> `callbacks[0]`; `onResumeNative` -> `callbacks[1]` | | V |
| 20 | `onWindowFocusChangedNative` -> `[6]`; `onContentRectChangedNative` -> `[18]`; `onWindowInsetsChangedNative` -> `[17]` | | V |
| 21 | `nativeInitClientSettings`, `nativePostClientSettingsLoadedInitialization3`; answer `gameActivity_onFlagsLoaded` or `_onFlagsFailed` | `NativeFlagsInitResult`, `NativeHelper` | V |
| 22 | `nativeGameGlobalInit()`, then `nativeAppBridgeV2StartAppWithParams(StartAppParams)` | `StartAppParams` | V |
| 23 | Callbacks `gameActivity_onEngineInitialized`, `_onAppReady`, `getDeviceStaticParams()`, ... | Tier 1 members | V |
| 24 | On a new surface, `nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams(Surface, PlatformParams)` | `PlatformParams` | V |
| 25 | `GameLoop`: `ALooper_pollOnce`, `APP_CMD_*`, graphics (EGL; Vulkan via `dlopen`) | EGL/GLES, `libvulkan.so` | V; order I |
| 26 | Input through `NativeInputInterface.nativePassInput*` (Roblox's path) | host pointer events | V |

Shutdown: `onPauseNative` -> `[3]`, `onStopNative` -> `[4]`, `onSurfaceDestroyedNative` -> `[10]`,
`terminateNativeCode` -> teardown `0x285c6bc`; `onSaveInstanceStateNative` -> `[2]`.

### 8.1 Failure modes, in order

1. Step 4: wrong `TPIDR_EL0` layout; `__stack_chk_fail` at once.
2. Step 5: a static constructor reaching an unimplemented import.
3. Step 6a/13: `FindClass`/`GetMethodID` returning null where the caller does not check. The
   `getProcessTimestamp` path checks; the `gGameActivityClassInfo` lookups in step 13 abort.
4. Step 13a: `ALooper_forThread()` null; `initializeNativeCode` returns 0 and startup fails
   silently.
5. Step 14: the cond wait makes a deadlock look like a hang; instrument it.
6. Steps 21-22: without the `fi.e` sequence the engine waits for flags forever, which looks like a
   graphics problem.

## 9. Reproduction

The analysis scripts lived in a session scratchpad and are not in the repo;
`jni-surface-lists.txt` is their output. In-repo re-derivations: `gen_dex_surface.py` (Section B x
dex -> `surface.rs`), `tools/dexdis.py`, `tools/call_sites.py`, `tools/init_reach.py`.
