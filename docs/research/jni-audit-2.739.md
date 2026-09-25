# JNI audit, 2.739.691: every Java member the engine can call, and what answers it here

Written 2026-09-25, after run w31 (Windows, in world in PS99): the owner pressed "Unlock chat",
the Lua thread died on `CallBooleanMethodV` of
`FacialAgeEstimationProtocol.isAvailable()Z` (refused as undecided), and the game froze. This
file is the decode of that class, and the audit that looks for the next such refusal before a
person finds it.

> **VERIFIED** = read out of the bytes of `Roblox-2.739.691.apk` (dex or `libroblox.so`) at a
> stated offset. **INFERRED** = reasoned, with the reasoning stated. "Link" addresses are
> `libroblox.so` ELF addresses.

## 1. `FacialAgeEstimationProtocol`, decoded

**The Java (VERIFIED, `classes2.dex`).** A Kotlin `object` (`<clinit>`: `new-instance; <init>;
sput-object INSTANCE`), and a lifecycle observer.

| member | body | reached by the engine? |
|---|---|---|
| `isAvailable()Z` | `sget-object personaSdk; if-eqz -> false; true` -- **only** whether the Persona SDK was set up. No camera, permission, Play-services or flag check. | yes: every `FacialAgeEstimationService:IsAvailable` |
| `setListener(J)V` | `nativeListenerPtr = Long.valueOf(ptr)` | yes, at startup |
| `startInquiry(String, String)V` | `personaSdk != null`: `isInInquiryFlow = true; personaSdk.launchInquiry(id, token, this)`; else log "PersonaSdk is not initialized" and `JNIInquiryResultListener.onError(ptr, id, that message)` | **no** while `isAvailable` is false (below) |
| `onCreate(LifecycleOwner)` | if the owner is an AppCompat activity (`j.b`): `Class.forName("com.roblox.client.personasdk.PersonaActivityResultLauncher")`, construct it reflectively, `onActivityCreated(activity)`, store it in `personaSdk`; `ClassNotFoundException` (and four other failures) are caught and logged ("Class not found: ...") | Java only |
| `onDestroy` | `personaSdk = null; isInInquiryFlow = false` | Java only |
| `onComplete/onCancel/onError` | the Persona result, to `JNIInquiryResultListener`'s natives with `nativeListenerPtr` | Java only |
| `isInInquiryFlow()Z` | the static | Java only (`ExperienceSession.shouldDisableExperienceIdleTimer`) |

Who calls `onCreate` on a device: `NativeHelper.a0`, on `MainGameActivity`'s lifecycle, adds
`INSTANCE` as an observer when `pk.u.b()` -- not Android TV (`android.software.leanback`), not
Chromebook -- after requesting the `personasdk` module (`pk.u.a`: immediately under flag `B4`,
deferred otherwise). The launcher is that module: `com.android.dynamic.apk.fused.modules=base,
personasdk` in the manifest, its classes in `classes3.dex` of this fused APK, downloaded on demand
on a Play install. It wraps the **Persona SDK** (`com.withpersona.sdk2`), which runs the face scan
in its own camera activity.

**The engine (VERIFIED, `libroblox.so`).**

* Startup, `0x235dac8`: resolve the class through the cached `ClassLoader`, read `INSTANCE`
  (`NewGlobalRef`), take `setListener` and **call it** (`0x235dc3c`), take `isAvailable` and
  `startInquiry`. The platform object is `AndroidFacialAgeEstimationProtocol` (vtable
  `0x6438568`: `+0x10` isAvailable `0x2f50bfc`, `+0x20` startInquiry `0x2f50c8c`).
* Lua `FacialAgeEstimationService:IsAvailable()` (`0x4849fa0`): false unless the flag byte at
  `0x6d00760` (`EnableFacialAgeEstimationService`) is set; then core `0x2f50248` -> the platform's
  `isAvailable` -> `CallBooleanMethod`. This is the call that died in w31, on the Lua thread (so
  every script waiting on it waited forever: the freeze).
* Lua `FacialAgeEstimationService:InquiryAsync{inquiryId, sessionToken}` (`0x484a050`) -> core
  `0x2f50260`, which **calls `isAvailable` first** and, on false, fails the inquiry with
  "Not available." without touching the platform. So with `isAvailable` false, Java
  `startInquiry` is unreachable.

**The answer here: false, the device's own.** This runtime executes no Persona SDK: there is no
module code, no camera, no activity for it to launch. That is precisely the state `onCreate` is
written for -- the reflective lookup fails, `personaSdk` stays null -- so `isAvailable` answers
false by its own test. It is declared that way (`Answer::StaticIsSet("personaSdk")` over an
`Answer::Assigned` field no scripted statement stores), not as a constant: the gate test
`facial_age_estimation_is_available_is_the_persona_sdk_field_test_in_the_apk` reads the method's
eight code units and the field out of the APK, and the unit test shows the answer follows the
field. Nothing about verification is bypassed or claimed: the engine is told the native age check
is not available on this device, which is true, and Roblox's own code chooses the fallback.
`startInquiry` stays a refusal naming itself (it cannot be reached; if it ever is, the engine
changed and must be re-read).

## 2. What the person will see now (the Lua side, VERIFIED from `InExperience.rbxm`)

The scripts are Luau bytecode in `assets/ExtraContent/models/InExperience/InExperience.rbxm`;
paths below are under `CorePackages/Workspace/Packages/_Workspace/`.

1. `ExpChat/EnableChatButton` ("Unlock chat") -> `TextChatService:SendEnableChatButtonClicked()`
   (the `The Chat Enable Button is clicked` line in w31) -> `AppLayout.onEnableChatActivated` ->
   `SocialUpsell/manuallyInvokeAmpUpsell("InExperienceTextChatUpsell")` ->
   `InExpAmpWizardController.OpenAmpWizardContainerInExp` (feature `TriggerAgeVerifyRecourse`,
   namespace `social/Upsells`).
2. **Server flags gate the wizard**: `InExperienceContainerScreenSizeReducer` and
   `MoveAmpUpsellOffNavigateDown` (Lua defaults false). Off: it only prints
   `InExpAmpWizardController: OpenAmpWizardContainerInExp called without necessary flags` and
   nothing appears.
3. On: a `CoreGui.AmpWizard` ScreenGui; `WizardContainer` asks
   `apis.roblox.com/access-management/v1/upsell-feature-access`. If the account's recourse is
   `AgeEstimation`: `FAEUpsellContainer` (a spinner), `POST .../age-verification-service/v1/
   persona-id-verification/start-verification` (the server's Persona `verificationLink`), then
   **`FAEWrapper.isAvailable()` -- the call that used to freeze -- now false**. The QR hand-off and
   the web wizard hand-off are behind flags defaulting false, so it shows the
   **`FAEDirectLinkUpsell` modal: "Let's check your age" / "This unlocks more games and features
   like chat." with "Continue" and "Cancel"**, while polling `.../verified-status` in the
   background (15 s x 40).
4. Other recourses lead elsewhere: `GovernmentId` -> ID verification, `ParentConsent`/`ParentLink`
   -> parent flow. Errors show "Age check error / Something went wrong".
5. **"Continue"** calls `AppLinking:openURL(verificationLink)`, which in-experience exists only
   when the server flag `InExperienceContainerAppLinking` is on, and is
   `LinkingService:OpenUrl(url)`. On Android the engine's `LinkingProtocolCore` sends that to the
   Java side as the MessageBus request `Linking`/`openURL` (INFERRED from the strings
   `openURLRequest`/`OpenURLUseRequestResponseEnabled` at `0x1e2fb48`), and the Java handler
   (`qm.e.j`, VERIFIED) starts an `ACTION_VIEW` intent: the system browser opens Persona's page.
   **This host does not install the Java `LinkingProtocol` (`qm.e`)**, so no handler answers that
   request: expect "Continue" to do nothing visible (no JNI call is made, so no thread dies). That
   is the next gap on this path; closing it means registering the `openURL` request handler the way
   `webview::WebViewProtocol::install` registers `isAvailable`, answering `{success: true}` and
   opening the URL in the host's browser.
6. The web branches that use a web view (`GenericWebPage`: `WebViewService:OpenWindow`, or
   `BrowserService:SendCommand{command="open"}`) are reached only with non-default flags or for
   other recourses. `WebViewService` is handled (the `WEBVIEW` lines in w31: `openWindow` opens a
   host browser window). `BrowserService.SendCommand` is **not** decoded here and refuses by name
   in the log (`webview.rs`, no thread dies).

## 3. The audit

### Method

* **The lookups.** `docs/research/jni-surface-lists.txt` section D is the static census of every
  `GetMethodID`/`GetStaticMethodID`/`GetFieldID`/`GetStaticFieldID` name and descriptor
  `libroblox.so` passes (418 triples, 104 classes), made on 2.738.1397. All 418 member names are
  still string literals of 2.739.691. The 2.739 generated surface (`jni/surface.rs`, class
  literals intersected with the dex) adds 38 classes the census did not attribute -- almost all
  Djinni `$CppProxy` and enum classes -- and the `LOOKUP` lines of the 59 run logs of this week
  (167 distinct member lookups) add what the engine really asked for on this host.
* **The answers.** A dump of the registry after `Jni::new`, `script::declare_script_classes`,
  `webview::declare_classes` and the hardware keyboard, i.e. every decided `Answer`, joined to
  the census by class, name and descriptor; the gate's own `define` calls (`Configuration`,
  `DisplayMetrics`, `Build`) counted as decided.
* **Which undecided member is really callable.** An undecided lookup is harmless until the
  member is *called*, and the engine can only call an instance method on an object it got. Each
  undecided engine-side class was traced in `libroblox.so` to its call site and trigger, and to
  how the receiver is obtained (a static `INSTANCE` read, a `NewObject`, or an object only a
  Java-called `native` hands over -- which this host never calls, making that path unreachable
  here), then decoded from the dex. Three parallel decodes did the tracing; their notes are
  summarised below with addresses.

Counts: of the 418 census rows, before this change **160 decided**, **190 undecided but declared**
(a call refuses by name); after it **169 decided, 181 undecided**; **68 not declared** (the lookup itself misses: 27 unresolved in the census, the rest
Android framework/JDK members the engine only looks up on paths below).

### Reachable and transcribed in this change

| class.method(sig) | reachable how | now | risk before |
|---|---|---|---|
| `FacialAgeEstimationProtocol.isAvailable()Z` | Unlock chat / any age-check upsell (Lua `FacialAgeEstimationService:IsAvailable`) | **false** (`personaSdk` never set: no Persona module) | **froze the game (w31)** |
| `ExperienceSession.shouldDisableExperienceIdleTimer()Z` (static) | `nativeActivity_onStop` (`0x2bf27ac`) inside an experience: minimising under the pause-in-background policy | **false** (no call ringing, no camera capture, no inquiry) | game thread dies on minimise |
| `PlatformSystemDialogHandler.isAvailable()Z` (via `IPlatformSystemDialogHandler`) | `AppPlatformQoSEmergency` (`0x227a96c`): a server-declared outage `Stop_Until` | **true** (`return true`) | thread dies when the servers declare an emergency |
| `PlatformSystemDialogHandler.open(SystemDialogRequest, ISystemDialogCallback)J` | same | **-1** (`currentActivity` null: the Java flags that store it default false) -> engine logs and plays on | same |
| `PlatformSystemDialogHandler.dismiss(J)V`, `dismissAll()V` | emergency end (`0x227ae2c`) | sink (no dialog is ever active) | same |
| `SystemDialogRequest.<init>(String x4, Z, Z, J)`, `ISystemDialogCallback$CppProxy.<init>(J)` | `NewObject` before `open` (`0x35df540`, `0x35dfb54`) | constructed (`nativeRef` kept) | `NewObject` refusal before `open` |
| `AppRatingPromptHandler.isAppRatingPromptAvailable()Z`, `showAppRatingPrompt()V` (static) | Lua `AppRatingPromptService`, asked by the app shell after leaving a game | **true** (`return true`); show is a no-op (no activity stored by `onCreate`) | thread dies on leaving a game when the shell asks |
| `GmaSdkAvailability.isInstalled()Z` (static) | `DefaultNativeAdsProtocol` (`0x2f5dbf8`): in-experience ads eligibility | **false** (`Class.forName` of the `gmasdk` module, absent from every dex) | thread dies when an experience asks for ads |

### Reachable, already decided, and faithful

| class.method | reachable how | answer | note |
|---|---|---|---|
| `NativeGLJavaInterface.promptNativePurchase*` (5 overloads) | `MarketplaceService:PromptNativePurchase*` ("Buy Robux" in the purchase prompt) | sink | With the live flags (`UseAndroidGooglePaymentsProtocol6` true, `...V2V2` false) the Java begins a payments-protocol purchase and never reports back when Play Billing is absent (`oj.m.a` fails its feature check) -- a Play-less phone's prompt also waits. **The prompt spins; no purchase happens.** Modelling the flag-dependent failure report (`nativeInGamePurchaseFinished(false, ...)`) is possible later; it would be a real failure, never a purchase. |
| `NativeGLJavaInterface.openNativeOverlay`, `saveImageToAlbum`, `exitGameWithError`, `gameDidLeave`, notifications | menus, screenshots, leave | sinks | the Java side's UI this host does not have |
| `FacialAgeEstimationProtocol.setListener(J)V` | startup | sink | |
| `SystemThemeProtocol`, `MediaCodecInfoUtils`, `FMOD`, `NativeQuoteInterface`, `CookieProtocol`, the `MessageBus`/`MemStorage` callbacks, `WebViewProtocol` | startup / web views | decided | earlier milestones |

### Undecided, and not reachable on this host (left refusing, by name)

| class.method | why unreachable here | if it ever is |
|---|---|---|
| `FacialAgeEstimationProtocol.startInquiry` | the core checks `isAvailable` (false) first | the `personaSdk == null` branch: log + `JNIInquiryResultListener.onError(ptr, id, "PersonaSdk is not initialized")` |
| `MediaPickerProtocolV2.INSTANCE`, `onMediaRequest(String,String,J)V` (Photo2Avatar camera/image picker, `AvatarCreationService:PromptSelectAvatarGenerationImageAsync`) | the engine creates `AndroidMediaPickerImpl` only in `initializeMediaPickerProtocol` (`0x2533f98`), which only the Java `onResume` calls; without it the engine reports `MediaInvalidPlatform` | `StaticInstance` + sink (`contextRef` null: the Java returns without answering) |
| `RecentlyPlayedWidgetHandler.INSTANCE`, `cacheRecentlyPlayedData` | needs `JNIWidgetDataProtocol.initializeProtocol`, called only when a home-screen widget exists | `StaticInstance` + sink |
| `IAPPurchaseManager.*` statics (9) | called only from inside the Java-called `IAPPurchaseManager.native*` natives | `getPlatformPaymentMethod`/`...ProviderType` = "GooglePlayStore"; the rest would NPE on the null `h` |
| `JNIAchievement.*` | only from `JNIAchievement.init`, the Play Games setup | Play Games absent |
| `FlagCacheUtils.setFlagCacheExpiry`, `OtaConfigHandler.clearPendingWorkerBlobFromNative` | only from unscripted Java-called natives | sinks (SharedPreferences writes) |
| `OtaConfigHandler.saveOtaConfigState` | an OTA DataModel patch deployment | sink (writes `ota_state` preferences nothing here reads) |
| `JNIBaseUrlSetter.setBaseUrl` | a deep link naming another base-URL host | sink |
| `JNIAppRestarter.restartApp` | a warm-start deep link to another host | **not a sink**: the Java starts an intent and `Runtime.exit(0)`; needs an exit seam |
| `WebRtcAudioManager.*` (voice) | engine `NewObject`s it when a voice device module is created (voice joined) | its Java constructor calls `nativeCacheAudioParameters`; a plain `NewInstance` would leave WebRTC's audio parameters unset -- decode before voice is enabled |
| Djinni `*$CppProxy` natives and the other `*platforminterface` handlers (`BugReporter`, `DeviceDisplay`, `PinShortcut`, `LocalStorage`, `ConnectivityV2`, `ExternalIdentity`, `DesignFoundations`, `AppAgeSignals`) | the engine logs "... not available on the current platform." when the Java side registers no implementation, and the host registers none | per class, when a run reaches one |

### The residual risk, ranked

1. **Voice chat** (`WebRtcAudioManager`): reachable the moment voice is enabled in a menu; the
   first call is `init()Z` on an object built without its native audio parameters. Decode next.
2. **`BrowserService.SendCommand`** and the **Linking `openURL`** request: no JNI refusal, but the
   buttons that use them do nothing here ("Continue" in the age check among them).
3. **Purchases**: the Robux prompt spins, as on a phone without Play; no refusal.
4. Everything in the unreachable table: refuses by name the day a scripted Java step makes it
   reachable, which is the intended failure.

## 4. Appendix: every census row, and the live lookups outside it

Generated by joining the census, the run logs' `LOOKUP` lines and the registry dump (method
above), after this change. "answer here" is the registry's `Answer`; `Unanswered` refuses when
called, "not declared" misses at lookup.

| class | lookup | member | evidence | live | answer here |
|---|---|---|---|---|---|
| `!absent-from-dex` | GetMethodID | `signalCameraDisconnected()V` | M/direct |  | not declared (lookup misses) |
| `!absent-from-dex` | GetMethodID | `signalFormatChanged(IIII)V` | M/direct |  | not declared (lookup misses) |
| `!absent-from-dex` | GetMethodID | `signalVideoFrameArrived(JLjava/nio/ByteBuffer;IIII)V` | M/direct |  | not declared (lookup misses) |
| `!absent-from-dex` | GetMethodID | `signalVideoStart()V` | M/direct |  | not declared (lookup misses) |
| `!absent-from-dex` | GetMethodID | `signalVideoStop()V` | M/direct |  | not declared (lookup misses) |
| `!ambiguous` | GetFieldID | `nameLjava/lang/String;` | M/direct |  | not declared (lookup misses) |
| `!ambiguous` | GetMethodID | `onItemSet(Ljava/lang/String;)V` | M/direct |  | ambiguous:fh/c$a=HostCallback,fh/c$b=HostCallback,fh/c$c=HostCallback,ri/a$a=HostCallback |
| `!ambiguous-kotlin-object` | GetStaticFieldID | `INSTANCE<unresolved>` | M/direct |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `<init>(J)V` | Hu/helper | yes | ambiguous:java/lang/Long=NewInstance,com/roblox/audio/AppRtcDeviceWrapper=NewInstance,com/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback$CppProxy=Construct([("nativeRef", "J")]),com/roblox/engine/jni/memstorage/Connection=Construct([("ref", "J")]),com/roblox/protocols/appagesignalsplatforminterface/generated/IAppAgeSignalsCore$CppProxy=Unanswered,com/roblox/protocols/appagesignalsplatforminterface/generated/IPlatformAppAgeSignals$CppProxy=Unanswered,com/roblox/protocols/bugreporterplatforminterface/generated/IBugReporterCore$CppProxy=Unanswered,com/roblox/protocols/bugreporterplatforminterface/generated/IPlatformBugReporter$CppProxy=Unanswered,com/roblox/protocols/connectivityv2platforminterface/generated/IConnectivityV2CoreListener$CppProxy=Unanswered,com/roblox/protocols/connectivityv2platforminterface/generated/IPlatformConnectivityV2$CppProxy=Unanswered,com/roblox/protocols/designfoundationsplatforminterface/generated/IDesignFoundationsCoreListener$CppProxy=Unanswered,com/roblox/protocols/designfoundationsplatforminterface/generated/IPlatformDesignFoundations$CppProxy=Unanswered,com/roblox/protocols/devicedisplayplatforminterface/generated/IDeviceDisplayHandlerCore$CppProxy=Unanswered,com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler$CppProxy=Unanswered,com/roblox/protocols/examplev2platforminterface/generated/IExampleHandlerCore$CppProxy=Unanswered,com/roblox/protocols/examplev2platforminterface/generated/IPlatformExampleHandler$CppProxy=Unanswered,com/roblox/protocols/externalidentityplatforminterface/generated/IExternalIdentityCoreListener$CppProxy=Unanswered,com/roblox/protocols/externalidentityplatforminterface/generated/IPlatformExternalIdentity$CppProxy=Unanswered,com/roblox/protocols/localstorageplatforminterface/generated/ILocalStorageHandlerCore$CppProxy=Unanswered,com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler$CppProxy=Unanswered,com/roblox/protocols/pinshortcutplatforminterface/generated/IPinShortcutHandlerCore$CppProxy=Unanswered,com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut$CppProxy=Unanswered,com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler$CppProxy=Unanswered,com/roblox/universalapp/messagebus/Connection=Construct([("a", "J")]),org/webrtc/voiceengine/WebRtcAudioManager=Unanswered |
| `!unresolved` | GetMethodID | `None()I` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID | `None()J` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID | `None()Ljava/lang/String;` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID | `None()Z` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetFieldID | `NoneLjava/lang/Object;` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetFieldID | `NoneLjava/lang/String;` | V/direct |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `config()Lcom/roblox/engine/jni/autovalue/RobloxTelemetryEventConfig;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `data()Lcom/roblox/engine/jni/autovalue/RobloxTelemetryEventData;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `dispose()V` | Hu/helper |  | ambiguous:org/webrtc/voiceengine/WebRtcAudioManager=Unanswered |
| `!unresolved` | GetMethodID(helper) | `getCallback()Lcom/roblox/universalapp/messagebus/NewRawCallback;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `getCallback()Lcom/roblox/universalapp/messagebus/RequestHandlerAsyncRaw;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `getCallback()Lcom/roblox/universalapp/messagebus/RequestHandlerRaw;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `getDomain()Ljava/lang/String;` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetFieldID(helper) | `nativeRef<unresolved>` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `ordinal()I` | Hu/helper |  | not declared (lookup misses) |
| `!unresolved` | GetMethodID(helper) | `platformParams()Lcom/roblox/engine/jni/model/PlatformParams;` | Hu/helper | yes | ambiguous:com/roblox/engine/jni/autovalue/InitParams=NewInstanceOf("com/roblox/engine/jni/model/PlatformParams"),com/roblox/engine/jni/autovalue/StartAppParams=Field("platformParams"),com/roblox/engine/jni/autovalue/StartGameParams=Field("platformParams") |
| `!unresolved` | GetMethodID(helper) | `surface()Landroid/view/Surface;` | Hu/helper | yes | ambiguous:com/roblox/engine/jni/autovalue/StartAppParams=Field("surface"),com/roblox/engine/jni/autovalue/StartGameParams=Field("surface") |
| `!unresolved` | GetMethodID(helper) | `vrContext()Landroid/app/Activity;` | Hu/helper | yes | ambiguous:com/roblox/engine/jni/autovalue/InitParams=Null,com/roblox/engine/jni/autovalue/StartAppParams=Null,com/roblox/engine/jni/autovalue/StartGameParams=Null |
| `android/app/ActivityThread` | GetStaticMethodID | `currentActivityThread()Landroid/app/ActivityThread;` | I/direct | yes | NewInstance |
| `android/app/ActivityThread` | GetStaticMethodID | `currentApplication()Landroid/app/Application;` | V/direct |  | NewInstanceOf("android/app/Application") |
| `android/app/ActivityThread` | GetMethodID | `getApplication()Landroid/app/Application;` | I/direct | yes | NewInstanceOf("android/app/Application") |
| `android/content/Context` | GetMethodID | `getResources()Landroid/content/res/Resources;` | M/direct | yes | NewInstanceOf("android/content/res/Resources") |
| `android/content/res/Configuration` | GetFieldID | `colorMode<unresolved>` | V/direct |  | Int(5) |
| `android/content/res/Configuration` | GetFieldID | `densityDpi<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `fontScale<unresolved>` | V/direct |  | Float(1.0) |
| `android/content/res/Configuration` | GetFieldID | `fontWeightAdjustment<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetMethodID | `getLocales()Landroid/os/LocaleList;` | V/direct | yes | NewInstanceOf("android/os/LocaleList") |
| `android/content/res/Configuration` | GetFieldID | `hardKeyboardHidden<unresolved>` | V/direct |  | Int(1) |
| `android/content/res/Configuration` | GetFieldID | `keyboard<unresolved>` | V/direct |  | Int(2) |
| `android/content/res/Configuration` | GetFieldID | `keyboardHidden<unresolved>` | V/direct |  | Int(1) |
| `android/content/res/Configuration` | GetFieldID | `mcc<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `mnc<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `navigation<unresolved>` | V/direct |  | Int(1) |
| `android/content/res/Configuration` | GetFieldID | `navigationHidden<unresolved>` | V/direct |  | Int(1) |
| `android/content/res/Configuration` | GetFieldID | `orientation<unresolved>` | V/direct |  | Int(2) |
| `android/content/res/Configuration` | GetFieldID | `screenHeightDp<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `screenLayout<unresolved>` | V/direct |  | Int(36) |
| `android/content/res/Configuration` | GetFieldID | `screenWidthDp<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `smallestScreenWidthDp<unresolved>` | V/direct |  | Int(0) |
| `android/content/res/Configuration` | GetFieldID | `touchscreen<unresolved>` | V/direct |  | Int(3) |
| `android/content/res/Configuration` | GetFieldID | `uiMode<unresolved>` | V/direct |  | Int(17) |
| `android/content/res/Resources` | GetMethodID | `getDisplayMetrics()Landroid/util/DisplayMetrics;` | M/direct | yes | NewInstanceOf("android/util/DisplayMetrics") |
| `android/os/LocaleList` | GetMethodID | `get(I)Ljava/util/Locale;` | V/direct | yes | NewInstanceOf("java/util/Locale") |
| `android/os/LocaleList` | GetMethodID | `size()I` | V/direct | yes | Int(1) |
| `android/util/DisplayMetrics` | GetFieldID | `density<unresolved>` | M/direct |  | Unanswered |
| `android/util/DisplayMetrics` | GetFieldID | `heightPixels<unresolved>` | M/direct |  | Unanswered |
| `android/util/DisplayMetrics` | GetFieldID | `widthPixels<unresolved>` | M/direct |  | Unanswered |
| `android/util/DisplayMetrics` | GetFieldID | `xdpi<unresolved>` | M/direct |  | Unanswered |
| `android/util/DisplayMetrics` | GetFieldID | `ydpi<unresolved>` | M/direct |  | Unanswered |
| `android/util/Log` | GetStaticMethodID | `getStackTraceString(Ljava/lang/Throwable;)Ljava/lang/String;` | V/direct |  | Text("") |
| `android/view/KeyEvent` | GetMethodID | `getAction()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getDeviceId()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getDownTime()J` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getEventTime()J` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getFlags()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getKeyCode()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getMetaState()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getModifiers()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getRepeatCount()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getScanCode()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getSource()I` | V/direct | yes | Unanswered |
| `android/view/KeyEvent` | GetMethodID | `getUnicodeChar()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getAction()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getActionButton()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getAxisValue(II)F` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getButtonState()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getClassification()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getDeviceId()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getDownTime()J` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getEdgeFlags()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getEventTime()J` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getFlags()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getHistoricalAxisValue(III)F` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getHistoricalEventTime(I)J` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getHistorySize()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getMetaState()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getPointerCount()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getPointerId(I)I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getRawX(I)F` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getRawY(I)F` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getSource()I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getToolType(I)I` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getXPrecision()F` | V/direct | yes | Unanswered |
| `android/view/MotionEvent` | GetMethodID | `getYPrecision()F` | V/direct | yes | Unanswered |
| `androidx/core/graphics/Insets` | GetFieldID | `bottom<unresolved>` | V/direct |  | Int(0) |
| `androidx/core/graphics/Insets` | GetFieldID | `left<unresolved>` | V/direct |  | Int(0) |
| `androidx/core/graphics/Insets` | GetFieldID | `right<unresolved>` | V/direct |  | Int(0) |
| `androidx/core/graphics/Insets` | GetFieldID | `top<unresolved>` | V/direct |  | Int(0) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `captionBar()I` | V/direct | yes | Int(4) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `displayCutout()I` | V/direct | yes | Int(128) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `ime()I` | V/direct | yes | Int(8) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `mandatorySystemGestures()I` | V/direct | yes | Int(32) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `navigationBars()I` | V/direct | yes | Int(2) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `statusBars()I` | V/direct | yes | Int(1) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `systemBars()I` | V/direct | yes | Int(7) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `systemGestures()I` | V/direct | yes | Int(16) |
| `androidx/core/view/WindowInsetsCompat$Type` | GetStaticMethodID | `tappableElement()I` | V/direct | yes | Int(64) |
| `com/appsflyer/R$attr / com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `maxHeight<unresolved>` | A2/direct |  | not declared (lookup misses) |
| `com/appsflyer/R$attr / com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `maxWidth<unresolved>` | A2/direct |  | not declared (lookup misses) |
| `com/appsflyer/R$attr / com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `minHeight<unresolved>` | A2/direct |  | not declared (lookup misses) |
| `com/appsflyer/R$attr / com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `minWidth<unresolved>` | A2/direct |  | not declared (lookup misses) |
| `com/google/android/gms/internal/measurement/g1` | GetMethodID | `apply()V` | D2/direct | yes | not declared (lookup misses) |
| `com/google/androidgamesdk/GameActivity` | GetMethodID | `finish()V` | V/direct | yes | Sink |
| `com/google/androidgamesdk/GameActivity` | GetMethodID | `getWaterfallInsets()Landroidx/core/graphics/Insets;` | V/direct | yes | NewInstanceOf("androidx/core/graphics/Insets") |
| `com/google/androidgamesdk/GameActivity` | GetMethodID | `getWindowInsets(I)Landroidx/core/graphics/Insets;` | V/direct | yes | NewInstanceOf("androidx/core/graphics/Insets") |
| `com/google/androidgamesdk/GameActivity` | GetMethodID | `setImeEditorInfoFields(III)V` | V/direct | yes | Sink |
| `com/google/androidgamesdk/GameActivity` | GetMethodID | `setWindowFlags(II)V` | V/direct | yes | Sink |
| `com/google/androidgamesdk/gametextinput/InputConnection` | GetMethodID | `restartInput()V` | D/direct | yes | Unanswered |
| `com/google/androidgamesdk/gametextinput/InputConnection` | GetMethodID | `setSoftKeyboardActive(ZI)V` | D/direct | yes | Unanswered |
| `com/google/androidgamesdk/gametextinput/InputConnection` | GetMethodID | `setState(Lcom/google/androidgamesdk/gametextinput/State;)V` | V/direct | yes | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetMethodID | `<init>(Ljava/lang/String;IIII)V` | D/direct |  | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetFieldID | `composingRegionEnd<unresolved>` | D2/direct |  | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetFieldID | `composingRegionStart<unresolved>` | D2/direct |  | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetFieldID | `selectionEnd<unresolved>` | D2/direct |  | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetFieldID | `selectionStart<unresolved>` | D2/direct |  | Unanswered |
| `com/google/androidgamesdk/gametextinput/State` | GetFieldID | `textLjava/lang/String;` | D/direct |  | Unanswered |
| `com/roblox/client/JNIAppRestarter` | GetStaticMethodID | `restartApp(Landroid/content/Context;Ljava/lang/String;)V` | V/direct | yes | Unanswered |
| `com/roblox/client/JNIBaseUrlSetter` | GetStaticMethodID | `setBaseUrl(Ljava/lang/String;)V` | V/direct | yes | Unanswered |
| `com/roblox/client/LocalStorageManager` | GetMethodID | `getAllocatableBytes()J` | V/direct | yes | Unanswered |
| `com/roblox/client/ads/GmaSdkAvailability` | GetStaticMethodID | `isInstalled()Z` | D/direct |  | Bool(false) |
| `com/roblox/client/flags/NativeFlagsInitResult` | GetMethodID | `<init>(I)V` | V/direct |  | NewInstance |
| `com/roblox/client/flags/NativeFlagsInitResult` | GetMethodID | `addBoolean(Ljava/lang/String;ZZ)V` | V/direct |  | Sink |
| `com/roblox/client/game/ExperienceSession` | GetStaticMethodID | `shouldDisableExperienceIdleTimer()Z` | D/direct |  | Bool(false) |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `getPlatformPaymentMethod()Ljava/lang/String;` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `getPlatformPaymentProviderType()Ljava/lang/String;` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `invokeStore(Ljava/lang/String;Ljava/lang/String;)Z` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `invokeStoreV2(Ljava/lang/String;JLjava/lang/String;)V` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `paymentsProtocolConcludeTransaction(Ljava/lang/String;JJ)Z` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `paymentsProtocolHandlePreparePaymentErrorCode(I)V` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `paymentsProtocolProcessPaymentCallback([JILjava/lang/String;Ljava/lang/String;)V` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `paymentsProtocolSetProgressBarDisplay(Z)V` | V/direct |  | Unanswered |
| `com/roblox/client/purchase/IAPPurchaseManager` | GetStaticMethodID | `setCheckoutSessionId(Ljava/lang/String;)Z` | V/direct |  | Unanswered |
| `com/roblox/client/scheduledwork/OtaConfigHandler` | GetStaticMethodID | `clearPendingWorkerBlobFromNative(Ljava/lang/String;)V` | D/direct |  | Unanswered |
| `com/roblox/client/scheduledwork/OtaConfigHandler` | GetStaticMethodID | `saveOtaConfigState(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V` | D/direct |  | Unanswered |
| `com/roblox/client/startup/FlagCacheUtils` | GetStaticMethodID | `setFlagCacheExpiry(Landroid/content/Context;I)V` | V/direct |  | Unanswered |
| `com/roblox/client/startup/MainGameActivity` | GetMethodID | `bootstrapTheApp()V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/MainGameActivity` | GetMethodID | `getNativeHelper()Lcom/roblox/client/startup/NativeHelper;` | D2/direct | yes | NewInstanceOf("com/roblox/client/startup/NativeHelper") |
| `com/roblox/client/startup/MainGameActivity` | GetMethodID | `openWebActivity(Ljava/lang/String;Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/MainGameActivity` | GetMethodID | `showLeaveAppPrompt()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/MainGameActivity` | GetMethodID | `syncCookiesFromEngine()V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/MainGameActivity / com/roblox/client/startup/MainGameActivity$Companion` | GetStaticMethodID | `getAppUpgradeKey()Ljava/lang/String;` | A2/direct | yes | not declared (lookup misses) |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_hideKeyboard()V` | D2/direct |  | HideKeyboard |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onAppReady(Ljava/lang/String;)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onDidLogInReceived(Ljava/lang/String;)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onDidLogOutReceived()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onDidSignUp(Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onDidSwitchAccountReceived()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onEngineInitialized()V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onExperienceStart()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onExperienceStop(D)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onFlagsFailed()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onFlagsLoaded(Ljava/nio/ByteBuffer;)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onGameLoaded(J)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onGameStreamingStatusChanged(Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onLuaAppDidReturn()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onLuaTextBoxChanged(Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onLuaTextBoxPropertyChanged()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onMotionEventListening(Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onRestartLuaApp()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onScanQrCode()V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onScreenOrientationChanged(IZ)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_onScreenshotReady(Ljava/lang/String;)V` | D2/direct |  | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_setAppUpgradeStatus(IILjava/lang/String;Ljava/lang/String;)V` | D2/direct | yes | Sink |
| `com/roblox/client/startup/NativeHelper` | GetMethodID | `gameActivity_showKeyboard(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V` | D2/direct |  | ShowKeyboard |
| `com/roblox/client/widgets/RecentlyPlayedWidgetHandler` | GetStaticFieldID | `INSTANCELcom/roblox/client/widgets/RecentlyPlayedWidgetHandler;` | D/direct |  | Unanswered |
| `com/roblox/client/widgets/RecentlyPlayedWidgetHandler` | GetMethodID | `cacheRecentlyPlayedData(Ljava/lang/String;Ljava/util/Map;)V` | D/direct |  | Unanswered |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `exitGameWithError(I)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `gameDidLeave()V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `gameLoadedCallback(J)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `getDeviceStaticParams()Lcom/roblox/engine/jni/model/DeviceStaticParams;` | V/direct | yes | NewInstanceOf("com/roblox/engine/jni/model/DeviceStaticParams") |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `getMobileAdvertisingId()V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `getWebViewUserAgent()V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `hideKeyboard()V` | V/direct | yes | HideKeyboard |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `listenToMotionEvents(Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onAppBridgeNotification(Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onAppShellReloadNeeded()V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onDataModelNotificationCallback(Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onExtendedAnalyticsRecvCallback([BI)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onLuaTextBoxChangedCallback(Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onLuaTextBoxPropertyChangedCallback()V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `onVrSessionStateUpdate(I)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `openNativeOverlay(Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `promptNativePurchase(JLjava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `promptNativePurchase(JLjava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `promptNativePurchaseWithPayload(JLjava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `promptNativePurchaseWithPaymentSessionId(JLjava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `promptNativePurchaseWithPaymentSessionId(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `saveImageToAlbum(Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `screenOrientationChanged(I)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/NativeGLJavaInterface` | GetStaticMethodID | `showKeyboard(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V` | V/direct | yes | ShowKeyboard |
| `com/roblox/engine/jni/NativeVideoInterface$VideoDeviceId` | GetMethodID | `<init>(ZLjava/lang/String;Ljava/lang/String;I)V` | V/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/locale/NativeLocaleJavaInterface` | GetStaticMethodID | `getGameLocale()Ljava/lang/String;` | V/direct | yes | Text("en_us") |
| `com/roblox/engine/jni/locale/NativeLocaleJavaInterface` | GetStaticMethodID | `getLocale()Ljava/lang/String;` | V/direct | yes | Text("en_us") |
| `com/roblox/engine/jni/locale/NativeLocaleJavaInterface` | GetStaticMethodID | `getRobloxLocale()Ljava/lang/String;` | V/direct | yes | Text("en_us") |
| `com/roblox/engine/jni/memstorage/Connection` | GetMethodID | `<init>(J)V` | V/direct | yes | Construct([("ref", "J")]) |
| `com/roblox/engine/jni/memstorage/Connection` | GetFieldID | `ref<unresolved>` | V/direct |  | Unanswered |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `batteryLowLjava/lang/Boolean;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `batteryPercentageLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `batterySaverModeLjava/lang/Boolean;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `chargeCounterLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `currentAverageLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `currentNowLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `energyCounterLjava/lang/Long;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `healthLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `pluggedLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `powerLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `presentLjava/lang/Boolean;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `statusLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `temperatureLjava/lang/Float;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/BatteryStatus` | GetFieldID | `voltageLjava/lang/Integer;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/model/ChannelRecord` | GetMethodID | `<init>(Ljava/lang/String;J)V` | V/direct |  | Unanswered |
| `com/roblox/engine/jni/model/ClientLocalFlags` | GetMethodID | `<init>()V` | V/direct |  | NewInstance |
| `com/roblox/engine/jni/model/ClientLocalFlags` | GetMethodID | `add(Ljava/lang/String;Ljava/lang/String;)V` | V/direct |  | Sink |
| `com/roblox/engine/jni/model/ClientLocalFlags` | GetMethodID | `size()I` | D/direct | yes | Int(0) |
| `com/roblox/engine/jni/model/NativeTextBoxInfo` | GetMethodID | `<init>(FFFFFZIIIIIIZZZ)V` | V/direct |  | Construct([("x", "F"), ("y", "F"), ("width", "F"), ("height", "F"), ("fontSize", "F"), ("multiline", "Z"), ("xAlignment", "I"), ("yAlignment", "I"), ("textColor", "I"), ("font", "I"), ("textInputType", "I"), ("returnKeyType", "I"), ("manualFocusRelease", "Z"), ("textWrapped", "Z"), ("editable", "Z")]) |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `getAppVersion()Ljava/lang/String;` | V/direct | yes | AppVersion |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `getFilesDir()Ljava/lang/String;` | V/direct | yes | Text("/data/data/com.roblox.client/files") |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `getLastLoggedInUser()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `getLastLoggedInUserId()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `sendSessionReport(Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | GetStaticMethodID | `setEventTrackingGoogleAnalytics(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;J)V` | V/direct | yes | Sink |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getAlternateName()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getDisplayName()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getHasRobloxSubscription()Z` | V/direct | yes | Bool(false) |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getIsUnder13()Z` | V/direct | yes | Bool(true) |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getMembershipType()I` | V/direct | yes | Int(0) |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getPlatformName()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getTheme()Ljava/lang/String;` | V/direct | yes | Text("Light") |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getUserId()J` | V/direct | yes | Long(-1) |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | GetStaticMethodID | `getUsername()Ljava/lang/String;` | V/direct | yes | Text("") |
| `com/roblox/engine/jni/util/NetworkUtils` | GetStaticMethodID | `getPublicIPv4Addresseses()Ljava/lang/String;` | V/direct | yes | Unanswered |
| `com/roblox/engine/jni/video/MediaCodecInfoUtils` | GetStaticMethodID | `getVideoCodecs()[Lcom/roblox/engine/jni/video/VideoCodecCapability;` | V/direct | yes | EmptyObjectArray |
| `com/roblox/engine/jni/video/MediaCodecInfoUtils` | GetStaticMethodID | `hevcHardwareEncodingSupported(III)Z` | V/direct | yes | Bool(false) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `codecLjava/lang/String;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `isEncoder<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `isHardware<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `levels[Ljava/lang/String;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `maxBitrate<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `maxFps<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `maxInstances<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `minBitrate<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `minFps<unresolved>` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/engine/jni/video/VideoCodecCapability` | GetFieldID | `profiles[Ljava/lang/String;` | D2/direct |  | not declared (lookup misses) |
| `com/roblox/platform/util/DeviceUtils` | GetStaticMethodID | `getScreenPhysicalSizeInMillimeters(Landroid/content/Context;)Landroid/graphics/Point;` | M/direct |  | not declared (lookup misses) |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformError` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsProvider;ILjava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformError` | GetFieldID(helper) | `providerLcom/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsProvider;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformError` | GetFieldID(helper) | `sdkErrorCode<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformError` | GetFieldID(helper) | `sdkErrorMessageLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformSuccess` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsProvider;Ljava/util/HashMap;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformSuccess` | GetFieldID(helper) | `payloadLjava/util/HashMap;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsPlatformSuccess` | GetFieldID(helper) | `providerLcom/roblox/protocols/appagesignalsplatforminterface/generated/AppAgeSignalsProvider;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/IPlatformAppAgeSignals` | GetMethodID(helper) | `getAgeSignal()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/appagesignalsplatforminterface/generated/IPlatformAppAgeSignals` | GetMethodID(helper) | `isAgeSignalAvailable()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/bugreporterplatforminterface/generated/IPlatformBugReporter` | GetMethodID(helper) | `isAvailable()Z` | HI/helper | yes | Unanswered |
| `com/roblox/protocols/bugreporterplatforminterface/generated/IPlatformBugReporter / com/roblox/protocols/bugreporterplatforminterface/generated/IPlatformBugReporter$CppProxy / com/roblox/protocols/devicedisplayplatforminterface/generated/IDeviceDisplayHandlerCore$CppProxy / com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler / com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler$CppProxy / com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut / com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut$CppProxy / com/roblox/protocols/systemdialog/PlatformSystemDialogHandler / com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler / com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler$CppProxy / com/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol` | GetMethodID | `isAvailable()Z` | A/direct | yes | not declared (lookup misses) |
| `com/roblox/protocols/connectivityv2platforminterface/generated/IPlatformConnectivityV2` | GetMethodID(helper) | `startMonitoring()V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/IPlatformConnectivityV2` | GetMethodID(helper) | `stopMonitoring()V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/connectivityv2platforminterface/generated/NetworkInterfaceType;[B[BIZLcom/roblox/protocols/connectivityv2platforminterface/generated/NetworkValidationState;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `interfaceTypeLcom/roblox/protocols/connectivityv2platforminterface/generated/NetworkInterfaceType;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `ipv4[B` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `ipv6[B` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `ipv6PrefixLength<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `pathSatisfied<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/connectivityv2platforminterface/generated/NetworkChangeReport` | GetFieldID(helper) | `validationLcom/roblox/protocols/connectivityv2platforminterface/generated/NetworkValidationState;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/ColorTokenVariants` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/designfoundationsplatforminterface/generated/RGBA;Lcom/roblox/protocols/designfoundationsplatforminterface/generated/RGBA;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/ColorTokenVariants` | GetFieldID(helper) | `darkLcom/roblox/protocols/designfoundationsplatforminterface/generated/RGBA;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/ColorTokenVariants` | GetFieldID(helper) | `lightLcom/roblox/protocols/designfoundationsplatforminterface/generated/RGBA;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/DesignTokens` | GetMethodID(helper) | `<init>(Ljava/util/HashMap;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/DesignTokens` | GetFieldID(helper) | `colorTokensLjava/util/HashMap;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/IPlatformDesignFoundations` | GetMethodID(helper) | `onTokensCleared()V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/IPlatformDesignFoundations` | GetMethodID(helper) | `onTokensUpdated(Lcom/roblox/protocols/designfoundationsplatforminterface/generated/DesignTokens;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/designfoundationsplatforminterface/generated/RGBA` | GetMethodID(helper) | `<init>(DDDD)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `getBrightness()F` | HI/helper |  | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `hasCapability(Lcom/roblox/protocols/devicedisplayplatforminterface/generated/DeviceDisplayCapability;)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `isAvailable()Z` | HI/helper | yes | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `setBrightness(F)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `setBrightnessToDefault()V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/devicedisplayplatforminterface/generated/IPlatformDeviceDisplayHandler` | GetMethodID(helper) | `setKeepAwake(Z)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/examplev2platforminterface/generated/IPlatformExampleHandler` | GetMethodID(helper) | `asyncPrintToNativeConsole(Ljava/lang/String;)I` | HI/helper |  | Unanswered |
| `com/roblox/protocols/examplev2platforminterface/generated/IPlatformExampleHandler` | GetMethodID(helper) | `printToNativeConsole(Ljava/lang/String;)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityCapabilities` | GetMethodID(helper) | `<init>(ZZ)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityCapabilities` | GetFieldID(helper) | `apple<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityCapabilities` | GetFieldID(helper) | `google<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityRequest` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProvider;Ljava/lang/String;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityRequest` | GetFieldID(helper) | `browserStartUrlLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityRequest` | GetFieldID(helper) | `nonceLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityRequest` | GetFieldID(helper) | `providerLcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProvider;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityResult` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProvider;Lcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityStatus;Lcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProofType;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityResult` | GetFieldID(helper) | `errorClassLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityResult` | GetFieldID(helper) | `proofTypeLcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProofType;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityResult` | GetFieldID(helper) | `providerLcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityProvider;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityResult` | GetFieldID(helper) | `statusLcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityStatus;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/IPlatformExternalIdentity` | GetMethodID(helper) | `acquireProof(JLcom/roblox/protocols/externalidentityplatforminterface/generated/ExternalIdentityRequest;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/IPlatformExternalIdentity` | GetMethodID(helper) | `cancel(J)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/externalidentityplatforminterface/generated/IPlatformExternalIdentity` | GetMethodID(helper) | `getCapabilities(J)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `deleteCurrentUserValues()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `deleteSecureValue(Ljava/lang/String;)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `deleteUserValues(J)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `getCurrentUser()J` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `getSecureValue(Ljava/lang/String;)Ljava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `getSecureValueForCurrentUser(Ljava/lang/String;)Ljava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `getSecureValueForUser(Ljava/lang/String;J)Ljava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `getUsers()Ljava/util/HashSet;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `setCurrentUser(J)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `setSecureValue(Ljava/lang/String;Ljava/lang/String;)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `setSecureValueForCurrentUser(Ljava/lang/String;Ljava/lang/String;)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/localstorageplatforminterface/generated/IPlatformLocalStorageHandler` | GetMethodID(helper) | `setSecureValueForUser(Ljava/lang/String;Ljava/lang/String;J)Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/mediapicker/MediaPickerProtocolV2` | GetMethodID | `onMediaRequest(Ljava/lang/String;Ljava/lang/String;J)V` | D/direct |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `getDesiredThumbnailFormat()Ljava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `isAvailable()Z` | HI/helper | yes | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `isPinExperienceV2Available()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `isRevealPinnedExperienceAvailable()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `pinExperience(JLjava/lang/String;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `pinExperienceV2(JJLjava/lang/String;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `revealPinnedExperience(J)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/pinshortcutplatforminterface/generated/IPlatformPinShortcut` | GetMethodID(helper) | `shouldShowLuaNotificationOnPinExperienceCompleted()Z` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler` | GetMethodID(helper) | `dismiss(J)V` | HI/helper | yes | Sink |
| `com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler` | GetMethodID(helper) | `dismissAll()V` | HI/helper | yes | Sink |
| `com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler` | GetMethodID(helper) | `isAvailable()Z` | HI/helper | yes | Bool(true) |
| `com/roblox/protocols/systemdialogplatforminterface/generated/IPlatformSystemDialogHandler` | GetMethodID(helper) | `open(Lcom/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest;Lcom/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback;)J` | HI/helper | yes | Long(-1) |
| `com/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback` | GetMethodID(helper) | `onCancelClicked(Lcom/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback` | GetMethodID(helper) | `onClosed(Lcom/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback` | GetMethodID(helper) | `onOkClicked(Lcom/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/ISystemDialogCallback` | GetMethodID(helper) | `onOpened(Lcom/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext` | GetMethodID(helper) | `<init>(JZ)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext` | GetFieldID(helper) | `dismissed<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogContext` | GetFieldID(helper) | `id<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetMethodID(helper) | `<init>(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZJ)V` | HI/helper |  | NewInstance |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `blockTimeInSeconds<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `cancelTextLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetMethodID | `getMessage()Ljava/lang/String;` | D/direct |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `messageLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `okTextLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `showProgressBar<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `showSpinner<unresolved>` | HI/helper |  | Unanswered |
| `com/roblox/protocols/systemdialogplatforminterface/generated/SystemDialogRequest` | GetFieldID(helper) | `titleLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetMethodID(helper) | `<init>(Ljava/lang/String;Ljava/util/HashSet;Ljava/lang/Integer;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `backendsLjava/util/HashSet;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `descriptionLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `eventNameLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `lastUpdatedLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `linksLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryConfig` | GetFieldID(helper) | `throttlingPercentageLjava/lang/Integer;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryData` | GetMethodID(helper) | `<init>(Ljava/util/HashMap;Ljava/util/HashSet;Ljava/lang/String;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryData` | GetFieldID(helper) | `customFieldsLjava/util/HashMap;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryData` | GetFieldID(helper) | `eventContextLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryData` | GetFieldID(helper) | `legacyOverrideTargetForEventIngestLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryData` | GetFieldID(helper) | `standardizedFieldsLjava/util/HashSet;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetMethodID(helper) | `<init>(Lcom/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValueType;Ljava/lang/Boolean;Ljava/lang/Long;Ljava/lang/Double;Ljava/lang/String;)V` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetFieldID(helper) | `boolValLjava/lang/Boolean;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetFieldID(helper) | `doubleValLjava/lang/Double;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetFieldID(helper) | `intValLjava/lang/Long;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetFieldID(helper) | `stringValLjava/lang/String;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValue` | GetFieldID(helper) | `valueTypeLcom/roblox/protocols/telemetrybindingsplatforminterface/generated/TelemetryFieldValueType;` | HI/helper |  | Unanswered |
| `com/roblox/protocols/webview/DomainAllowListChecker$Callback` | GetMethodID | `onResult(I)V` | M/direct |  | not declared (lookup misses) |
| `com/roblox/universalapp/achievement/JNIAchievement` | GetStaticMethodID | `grantAchievementForNativeAsync(Ljava/lang/String;J)Z` | V/direct |  | Unanswered |
| `com/roblox/universalapp/achievement/JNIAchievement` | GetStaticMethodID | `hasAchievedForNativeAsync(Ljava/lang/String;J)Z` | V/direct |  | Unanswered |
| `com/roblox/universalapp/appratingprompt/AppRatingPromptHandler` | GetStaticMethodID | `isAppRatingPromptAvailable()Z` | D/direct |  | Bool(true) |
| `com/roblox/universalapp/appratingprompt/AppRatingPromptHandler` | GetStaticMethodID | `showAppRatingPrompt()V` | D/direct |  | Sink |
| `com/roblox/universalapp/cookie/CookieProtocol` | GetStaticMethodID | `setCookie(Ljava/lang/String;Ljava/lang/String;)V` | V/direct | yes | CookieProtocolSetCookie |
| `com/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol` | GetMethodID | `setListener(J)V` | D/direct | yes | Sink |
| `com/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol` | GetMethodID | `startInquiry(Ljava/lang/String;Ljava/lang/String;)V` | D/direct | yes | Unanswered |
| `com/roblox/universalapp/logging/LoggingProtocol` | GetStaticMethodID | `getProcessTimestamp()J` | V/direct | yes | Unanswered |
| `com/roblox/universalapp/messagebus/Connection` | GetMethodID | `<init>(J)V` | V/direct | yes | Construct([("a", "J")]) |
| `com/roblox/universalapp/messagebus/MessageBus$*` | GetMethodID | `run(Ljava/lang/String;)Z` | M/direct |  | not declared (lookup misses) |
| `com/roblox/universalapp/messagebus/MessageBus$a` | GetMethodID | `run(Ljava/lang/String;)V` | D2/direct |  | HostCallback |
| `com/roblox/universalapp/messagebus/MessageBus$b` | GetMethodID | `run(Ljava/lang/String;)Ljava/lang/String;` | D2/direct |  | HostRequest |
| `com/roblox/universalapp/messagebus/MessageBus$c / com/roblox/universalapp/messagebus/RequestHandlerAsyncRaw` | GetMethodID | `run(Ljava/lang/String;Ljava/lang/String;)V` | A2/direct |  | not declared (lookup misses) |
| `com/roblox/universalapp/systemtheme/SystemThemeProtocol` | GetStaticMethodID | `getSystemTheme()I` | D/direct | yes | Int(3) |
| `com/roblox/universalapp/systemtheme/SystemThemeProtocol` | GetStaticMethodID | `isSystemThemeAvailable()Z` | D/direct | yes | Bool(true) |
| `com/snapchat/djinni/NativeObjectManager` | GetMethodID | `getClassLoader()Ljava/lang/ClassLoader;` | V/direct | yes | NewInstanceOf("java/lang/ClassLoader") |
| `java/lang/Boolean` | GetMethodID | `booleanValue()Z` | M/direct |  | Bool(false) |
| `java/lang/ClassLoader` | GetMethodID | `findClass(Ljava/lang/String;)Ljava/lang/Class;` | V/direct | yes | ResolveClass |
| `java/lang/ClassLoader` | GetMethodID | `getClassLoader()Ljava/lang/ClassLoader;` | I/direct | yes | NewInstanceOf("java/lang/ClassLoader") |
| `java/lang/ClassLoader` | GetMethodID | `loadClass(Ljava/lang/String;)Ljava/lang/Class;` | V/direct | yes | ResolveClass |
| `java/lang/Double` | GetMethodID(helper) | `doubleValue()D` | HI/helper |  | Double(0.0) |
| `java/lang/Float` | GetMethodID | `floatValue()F` | M/direct |  | Float(0.0) |
| `java/lang/Integer` | GetMethodID | `intValue()I` | M/direct |  | Int(0) |
| `java/lang/Long` | GetMethodID | `<init>(J)V` | V/direct | yes | NewInstance |
| `java/lang/Long` | GetMethodID | `longValue()J` | M/direct |  | Long(0) |
| `java/lang/Runnable` | GetMethodID | `run()V` | M/direct |  | not declared (lookup misses) |
| `java/lang/String` | GetMethodID | `getBytes(Ljava/lang/String;)[B` | M/direct |  | StringBytes |
| `java/lang/String` | GetMethodID | `onSetCookie([Ljava/lang/String;Ljava/lang/String;)V` | I/direct | yes | not declared (lookup misses) |
| `java/lang/ref/WeakReference` | GetMethodID(helper) | `<init>(Ljava/lang/Object;)V` | HI/helper |  | not declared (lookup misses) |
| `java/lang/ref/WeakReference` | GetMethodID(helper) | `get()Ljava/lang/Object;` | HI/helper |  | not declared (lookup misses) |
| `java/util/HashMap` | GetMethodID | `<init>()V` | V/direct |  | NewInstance |
| `java/util/HashMap` | GetMethodID | `<init>(I)V` | V/direct |  | NewInstance |
| `java/util/HashMap` | GetMethodID(helper) | `entrySet()Ljava/util/Set;` | HI/helper |  | Unanswered |
| `java/util/HashMap` | GetMethodID | `put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;` | V/direct |  | Null |
| `java/util/HashMap` | GetMethodID(helper) | `size()I` | HI/helper | yes | Int(0) |
| `java/util/HashSet` | GetMethodID(helper) | `<init>()V` | HI/helper |  | not declared (lookup misses) |
| `java/util/HashSet` | GetMethodID(helper) | `add(Ljava/lang/Object;)Z` | HI/helper |  | not declared (lookup misses) |
| `java/util/HashSet` | GetMethodID(helper) | `iterator()Ljava/util/Iterator;` | HI/helper |  | not declared (lookup misses) |
| `java/util/HashSet` | GetMethodID(helper) | `size()I` | HI/helper | yes | not declared (lookup misses) |
| `java/util/Iterator` | GetMethodID(helper) | `next()Ljava/lang/Object;` | HI/helper |  | not declared (lookup misses) |
| `java/util/List` | GetMethodID | `None()Ljava/util/List;` | I/direct |  | not declared (lookup misses) |
| `java/util/List` | GetMethodID | `get(I)Ljava/lang/Object;` | M/direct | yes | ListGet |
| `java/util/List` | GetMethodID | `toArray()[Ljava/lang/Object;` | V/direct |  | ListToArray |
| `java/util/Locale` | GetMethodID | `getCountry()Ljava/lang/String;` | V/direct | yes | Text("US") |
| `java/util/Locale` | GetMethodID | `getLanguage()Ljava/lang/String;` | V/direct | yes | Text("en") |
| `java/util/Locale` | GetMethodID | `getScript()Ljava/lang/String;` | V/direct | yes | Text("") |
| `java/util/Locale` | GetMethodID | `getVariant()Ljava/lang/String;` | V/direct | yes | Text("") |
| `java/util/Map$Entry` | GetMethodID(helper) | `getKey()Ljava/lang/Object;` | HI/helper |  | Null |
| `java/util/Map$Entry` | GetMethodID(helper) | `getValue()Ljava/lang/Object;` | HI/helper |  | Null |
| `java/util/Set` | GetMethodID(helper) | `iterator()Ljava/util/Iterator;` | HI/helper |  | not declared (lookup misses) |
| `org/webrtc/voiceengine/WebRtcAudioManager` | GetMethodID(helper) | `init()Z` | HD/helper |  | Unanswered |
| `org/webrtc/voiceengine/WebRtcAudioManager` | GetMethodID(helper) | `isCommunicationModeEnabled()Z` | HD/helper |  | Unanswered |
| `org/webrtc/voiceengine/WebRtcAudioManager` | GetMethodID(helper) | `isDeviceBlacklistedForOpenSLESUsage()Z` | HD/helper |  | Unanswered |
| `org/webrtc/voiceengine/WebRtcAudioManager` | GetMethodID(helper) | `setMicrophoneMute(Z)V` | HD/helper |  | Unanswered |

Live lookups not in the 2.738 census (class from the registry by name and descriptor):

| lookup | member | registry classes declaring it, and their answers |
|---|---|---|
| GetMethodID | `accessCode()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `baseURL()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/InitParams`=Text("https://www.roblox.com") |
| GetMethodID | `callId()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `conversationId()J` | `com/roblox/engine/jni/autovalue/StartGameParams`=Long(0) |
| GetMethodID | `edit()Landroid/content/SharedPreferences$Editor;` | `android/content/SharedPreferences`=PreferencesEdit |
| GetMethodID | `eventId()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `gameId()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `gameIdToExclude()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `gameJoinContext()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `getSharedPreferences(Ljava/lang/String;I)Landroid/content/SharedPreferences;` | `android/content/Context`=GetSharedPreferences |
| GetMethodID | `isTablet()Z` | `com/roblox/engine/jni/autovalue/InitParams`=Bool(false) |
| GetMethodID | `isUnder13()Z` | `com/roblox/engine/jni/autovalue/StartGameParams`=Bool(false) |
| GetMethodID | `isVrDevice()Z` | `com/roblox/engine/jni/autovalue/InitParams`=Bool(false) |
| GetMethodID | `isoContext()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `joinAttemptId()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `joinAttemptOrigin()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("PinnedShortcut") |
| GetMethodID | `joinRequestType()I` | `com/roblox/engine/jni/autovalue/StartGameParams`=Int(0) |
| GetMethodID | `launchData()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `linkCode()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `placeId()J` | `com/roblox/engine/jni/autovalue/StartGameParams`=Field("placeId") |
| GetMethodID | `putString(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;` | `android/content/SharedPreferences$Editor`=EditorPutString |
| GetMethodID | `referralPage()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `referredByPlayerId()J` | `com/roblox/engine/jni/autovalue/StartGameParams`=Long(0) |
| GetMethodID | `reservedServerAccessCode()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Text("") |
| GetMethodID | `userAgent()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/InitParams`=Text("Roblox/Android") |
| GetMethodID | `userId()J` | `com/roblox/engine/jni/autovalue/StartGameParams`=Field("userId") |
| GetMethodID | `username()Ljava/lang/String;` | `com/roblox/engine/jni/autovalue/StartGameParams`=Field("username") |
| GetStaticMethodID | `checkInit()Z` | `org/fmod/FMOD`=StaticIsSet("gContext") |
| GetStaticMethodID | `identityHashCode(Ljava/lang/Object;)I` | `java/lang/System`=IdentityHash |
| GetStaticMethodID | `isDebuggerConnected()Z` | `android/os/Debug`=Bool(false) |
| GetStaticMethodID | `requestResponse([B)[B` | `com/roblox/engine/jni/NativeQuoteInterface`=QuoteResponse |
| GetStaticMethodID | `supportsAAudio()Z` | `org/fmod/FMOD`=Bool(true) |
| GetStaticMethodID | `supportsLowLatency()Z` | `org/fmod/FMOD`=Bool(false) |
