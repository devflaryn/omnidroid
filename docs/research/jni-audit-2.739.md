# JNI audit: Java members the engine can call, and what answers them

Written 2026-09-25 after run w31 (Windows, in a world): pressing "Unlock chat" killed the Lua
thread on `FacialAgeEstimationProtocol.isAvailable()Z` (then undecided) and froze the game.

**Build.** Measured on the modified 2.739.691 build; not re-measured on stock. That APK is gone;
the fixture is now the stock 2.738.1397. Every `libroblox.so` address below (`0x...`) is a
2.739.691 link address and does not apply to the stock binary. The Java side was re-checked on
2026-09-26: the stock `classes2.dex` declares every method in the transcribed table below, with
the same descriptors, and stock `libroblox.so` names all five classes. The Java bodies and the
answers below have not been re-decoded on stock.

## 1. `FacialAgeEstimationProtocol`

Java (`classes2.dex`): a Kotlin `object`. `isAvailable()Z` returns whether the static `personaSdk`
is set, and nothing else. `onCreate(LifecycleOwner)` sets `personaSdk` by reflectively loading
`com.roblox.client.personasdk.PersonaActivityResultLauncher` from the on-demand `personasdk`
module and logs "Class not found" when it is absent. `startInquiry(String, String)` launches the
Persona SDK, or reports "PersonaSdk is not initialized" through `JNIInquiryResultListener.onError`.

Engine (2.739.691 addresses): startup `0x235dac8` reads `INSTANCE`, calls `setListener`, and takes
the `isAvailable`/`startInquiry` ids. Lua `FacialAgeEstimationService:IsAvailable()` (`0x4849fa0`)
is gated by the flag `EnableFacialAgeEstimationService`, then calls Java `isAvailable`.
`InquiryAsync` (`0x484a050`) calls `isAvailable` first and fails with "Not available." when
false, so `startInquiry` is unreachable while `isAvailable` is false.

Answer here (`classes.rs`): `isAvailable` = `Answer::StaticIsSet("personaSdk")`, a field no
scripted statement stores, so false, which is what the Java computes on a device without the
Persona module. `setListener` is a sink; `startInquiry` stays a refusal that names itself.

## 2. The Lua side

From `assets/ExtraContent/models/InExperience/InExperience.rbxm`: "Unlock chat" opens the age-check
upsell only when server flags allow it. With `isAvailable` false the upsell shows the
`FAEDirectLinkUpsell` modal ("Let's check your age"). Its "Continue" calls `LinkingService:OpenUrl`,
which reaches Java as the MessageBus request `Linking`/`openURL`. This host installs no `openURL`
handler (none in `jni/webview.rs` as of 2026-09-26), so "Continue" does nothing visible; no JNI
call is made and no thread dies. `WebViewService:OpenWindow` is handled; `BrowserService
.SendCommand` refuses by name.

## 3. The audit

### Method

The census is `jni-surface-lists.txt` Section D (418 name/descriptor triples in 104 classes, from
2.738.1397; all 418 names were still literals in 2.739.691), plus the generated surface
(`jni/surface.rs`) and the `LOOKUP` lines of that week's run logs. The registry after `Jni::new`
and the script/webview declarations gives every decided `Answer`. Each undecided engine-side class
was traced to its call site and to how the engine obtains the receiver: an instance method is
callable only on an object the engine gets (a static `INSTANCE`, a `NewObject`, or an object only
a Java-called native hands over, which this host never calls).

Counts after the change: 169 decided, 181 undecided but declared (a call refuses by name), 68 not
declared (the lookup misses).

### Reachable and transcribed

| Method | Reached by | Answer (`classes.rs`) |
|---|---|---|
| `FacialAgeEstimationProtocol.isAvailable()Z` | age-check upsell, Unlock chat | `StaticIsSet("personaSdk")`: false |
| `ExperienceSession.shouldDisableExperienceIdleTimer()Z` (static) | `nativeActivity_onStop` in an experience (minimise under the pause policy) | `Bool(false)` |
| `PlatformSystemDialogHandler.isAvailable()Z` | `AppPlatformQoSEmergency`: a server-declared outage | `Bool(true)` (the Java returns true) |
| `PlatformSystemDialogHandler.open(SystemDialogRequest, ISystemDialogCallback)J` | same | `Long(-1)` (no current activity); the engine logs and continues |
| `PlatformSystemDialogHandler.dismiss(J)V`, `dismissAll()V` | outage end | sinks |
| `SystemDialogRequest.<init>`, `ISystemDialogCallback$CppProxy.<init>(J)` | `NewObject` before `open` | constructed, `nativeRef` kept |
| `AppRatingPromptHandler.isAppRatingPromptAvailable()Z`, `showAppRatingPrompt()V` (static) | `AppRatingPromptService`, after leaving a game | `Bool(true)`; show is a sink |
| `GmaSdkAvailability.isInstalled()Z` (static) | in-experience ads eligibility | `Bool(false)` (the `gmasdk` module is absent) |

Before the change each of these refused and killed its thread. `tools/mutate.py` (`jniaudit-*`
rows) checks the transcriptions.

### Already decided

`NativeGLJavaInterface.promptNativePurchase*` are sinks: the Robux prompt spins, as on a phone
without Play Billing, and no purchase happens. `openNativeOverlay`, `saveImageToAlbum`,
`exitGameWithError`, `gameDidLeave` and notifications are sinks. `SystemThemeProtocol`,
`MediaCodecInfoUtils`, FMOD, `NativeQuoteInterface`, `CookieProtocol`, the MessageBus/MemStorage
callbacks and `WebViewProtocol` were decided earlier.

### Undecided and unreachable here (left refusing)

| Method | Why unreachable |
|---|---|
| `FacialAgeEstimationProtocol.startInquiry` | `isAvailable` is false |
| `MediaPickerProtocolV2` | engine side created only from the Java `onResume` |
| `RecentlyPlayedWidgetHandler` | needs a home-screen widget |
| `IAPPurchaseManager.*` statics | called only from inside Java-called natives |
| `JNIAchievement.*` | Play Games setup only |
| `FlagCacheUtils`, `OtaConfigHandler`, `JNIBaseUrlSetter` | unscripted natives, OTA, deep links |
| `JNIAppRestarter.restartApp` | warm-start deep link; not a sink: the Java exits the process |
| `WebRtcAudioManager.*` | voice; its Java constructor calls `nativeCacheAudioParameters`, so a plain instance would leave WebRTC's audio parameters unset |
| Djinni `*$CppProxy` and other `*platforminterface` handlers | the engine logs "not available on the current platform" |

### Residual risk, ranked

1. Voice chat (`WebRtcAudioManager`): reachable once voice is enabled; decode first.
2. `BrowserService.SendCommand` and the `openURL` request: buttons that do nothing here.
3. Purchases: the prompt spins; no refusal.
4. The unreachable table: each refuses by name if a scripted Java step makes it reachable.

The per-row appendix (all 418 census rows joined to the registry) was dropped on 2026-09-26; it
was 2.739.691-specific and is reproducible from Section D and a registry dump.
