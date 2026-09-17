# JNI Surface of `libroblox.so` — can Omnidroid run it with no ART?

**Target:** `lib/arm64-v8a/libroblox.so` from `Roblox-2.738.1397.apk` (109,193,800 B, stripped, `ET_DYN`, `EM_AARCH64`)
**Companion raw lists:** [`jni-surface-lists.txt`](jni-surface-lists.txt) (2,254 lines, sections A1–M)
**Prerequisite reading:** [`apk-analysis.md`](apk-analysis.md)
**Analysis date:** 2026-09-18

**Scope.** Stock Roblox + AGDK + AndroidX only. `libzstd-jni-1.5.7-6.so`, `com.roblox.gloop.*`,
`classes4.dex` and `assets/gloop/` are **excluded** from every requirement count below; where they
do appear it is flagged explicitly (§7).

> **VERIFIED** = read out of the bytes of this APK at a stated offset/pc.
> **INFERRED** = reasoned, with the reasoning stated. Nothing else is asserted.

---

## 0. Headline numbers and verdict

| Question | Answer |
|---|---:|
| Distinct `JNINativeInterface` slots the engine actually dereferences | **59** of 233 (170 never touched, 4 reserved) |
| JNI call sites found | **943** |
| Distinct `JNIInvokeInterface` (`JavaVM`) slots used | **2** (`GetEnv`, `AttachCurrentThread`) |
| Java class-name string literals in the whole binary | **128** (complete; none outside `.rodata`) |
| JNI type-descriptor string literals in the whole binary | **135** (complete) |
| Distinct Java classes with at least one member lookup | **104** |
| **Distinct Java members the native shim must answer** | **409** (296 methods + 113 fields) |
| … of which lazily-initialised Djinni protocol bridges (not startup) | 109 members / 26 classes |
| … non-Djinni "real" surface | **300 members / 78 classes** |
| Native methods declared in dex (whole APK) | 706 (662 statically exported, 44 `RegisterNatives`-only) |
| Native methods on the verified startup path to a surface | **~34** (§4) |
| Java classes that must exist for a first frame | **~14** (§3, ranked) |

**Verdict (Q5): a real dex interpreter is NOT required.** Nothing in `libroblox.so` forces dex
execution. There is no reflection, no `dalvik/system/*`, no `Class.forName`, no
`DexClassLoader`, no `DefineClass`, no Java-side HTTP/file/SQLite/WebView on the engine's JNI path.
The 409-member surface is small, hand-writable, and 90 % of it is *Roblox's own* thin Kotlin shell
whose behaviour Omnidroid gets to define. The single structural caveat is that the **Java side is
the initiator for a large part of startup** (§4/§8): `libroblox.so` will not reach a first frame by
itself — the host must *drive* it with a scripted sequence of ~34 JNI downcalls that ART would
normally be making from `MainGameActivity`/`NativeHelper`/`fi.e`. That is an orchestration problem,
not an interpretation problem.

**The real blocker is not JNI at all.** 1,282 `MRS Xt, TPIDR_EL0` instructions, 1,276 of which
immediately read `[Xt, #0x28]` — bionic's `TLS_SLOT_STACK_GUARD`. Every stack-protected function in
the binary reads the bionic thread-pointer layout directly (§6.4). Omnidroid must program
`TPIDR_EL0` per guest thread before a single `Java_*` export can return.

---

## 1. Method — how the JNI surface was extracted from a stripped 109 MB binary

Standard binary tooling is absent on this host (see `apk-analysis.md` §0). Everything below was
produced by purpose-written Python in the scratchpad, reusing the prior agent's `elf.py`
(ELF64 + APS2 packed-relocation decoder), `dex.py` and `zipscan.py`.

**Disassembler: `capstone` 5.0.7 (`pip`-installed, available).** Reported as requested. I did
*not* fall back to a hand decoder for the final numbers, though an initial hand decoder
(`jni/arm.py`) was written and its results were used as a cross-check; the two disagreed
(the hand decoder did not model `LDUR`/`x29`-relative loads, producing 9 false positives on the
`reserved0..3` slots which are by definition uncallable). Capstone with
`detail=True` plus a "invalidate every register in `regs_access().write`" default rule removed all
9 — **zero** hits on reserved slots in the final run, which is itself a sanity signal.

**Pipeline (all scripts in the scratchpad `jni/` directory):**

1. **`step3.py` — exact function boundaries.** `.eh_frame_hdr` at file offset 17,510,296 is
   version 1, `eh_frame_ptr_enc=0x1b`, `fde_count_enc=0x03`, `table_enc=0x3b` (datarel|sdata4) and
   carries a sorted binary-search table of **245,117 FDEs** covering `0x1d95980`–`0x62d5b5c`.
   Decoding it gives every function start in `.text` (72,614,148 B = 18,153,537 instructions).
   VERIFIED.
2. **`step1/step2.py` — string mining.** 154,426 NUL-terminated printable strings in `.rodata`.
   Classified with `^\((\[*([ZBCSIJFD]|L[\w/$]+;))*\)(V|\[*…)$` for descriptors and a
   package-rooted regex for class names → **135 descriptors, 128 class names**. A second sweep over
   **every** `PROGBITS`/`.data` section found **zero** additional class-name or descriptor strings,
   so both lists are complete for this binary.
3. **`xref.py` — string cross-references.** All 684,786 `ADRP` instructions decoded with numpy;
   filtered to the ~200 pages containing the 263 interesting strings; then `ADRP`+`ADD` pairing
   within 16 instructions → **533 reference sites in 200 functions**, covering 250 of the 263
   strings (13 strings are unreferenced from code).
4. **`cs_interp.py` / `run2.py` — interprocedural `JNIEnv` taint.** A `JNIEnv*` is, by the JNI ABI,
   `x0` of every `Java_*` export. Seeds = the **539 `Java_*` exports** + `JNI_OnLoad`
   (`x0` = `JavaVM*`, tracked separately) + the **26 `RegisterNatives` function pointers** (§5.2).
   Straight-line symbolic execution per function tracks registers, the stack frame, `ADRP/ADD`
   constants, global loads/stores, and propagates the taint through `BL` whenever `x0` holds the
   tainted value. Fixpoint: **683 env-tainted functions**; plus the 154 string-anchored functions
   that the taint does not reach (env arrives in a struct field) = **854 functions analysed**.
   A JNI call is recognised **only** as `ldr Xb,[ENV]` → `ldr Xt,[Xb,#imm]` → `blr Xt`, i.e. a
   double dereference whose base is a tainted `JNIEnv*`. This is what excludes the ~1 M C++
   virtual calls that have an identical instruction shape.
5. **Offset→function mapping.** Built from the *declaration order* of
   `struct JNINativeInterface` in the NDK `jni.h`: 4 reserved slots, then `GetVersion`,
   `DefineClass`, `FindClass`, …, ending at `GetObjectRefType` — **233 entries, table size
   0x748**, offset = index × 8 (`jni/jnitab.py`). Key values used throughout:
   `FindClass` 6/0x30, `GetObjectClass` 31/0xf8, `GetMethodID` 33/0x108, `GetFieldID` 94/0x2f0,
   `GetStaticMethodID` 113/0x388, `GetStaticFieldID` 144/0x480, `NewStringUTF` 167/0x538,
   `RegisterNatives` 215/0x6b8, `GetJavaVM` 219/0x6d8, `ExceptionCheck` 228/0x720.
   **The mapping validated itself:** of the 943 detected sites, every site that carries a literal
   string argument lands on exactly one of six offsets — 0x30 (class name in `x1`), 0x108/0x388
   (name in `x2`, descriptor in `x3`), 0x2f0/0x480 (same shape), 0x538 (plain C string). No
   literal-string site landed on any other offset. An independent jni.h-order error would have to
   permute six offsets consistently with their argument shapes; it does not happen by accident.
6. **Class attribution.** Member lookups were tied to their class by (a) in-binary dataflow from
   the `FindClass` return value through `NewGlobalRef`/`NewLocalRef`/`NewWeakGlobalRef`/
   `GetObjectClass`, recursive through `BL` return values and global caches; (b) unique
   `(name,descriptor)` match against the 99 candidate classes present in the dex; (c) unique match
   against all 26,620 dex classes; (d) sole-`FindClass`-in-function inference; (e) inspection.
   Each row in the lists file is tagged `V/I/D/D2/A/A2/H/HI/HD/M` accordingly.
7. **Dex cross-check.** `dexmeth.py` parses all four dex files (26,620 classes, **706 native
   methods** — matching the prior analysis exactly) with full descriptors. Of the class-attributed
   member lookups, **107 were confirmed to exist verbatim in the dex**, 9 "mismatched" — all 9
   explained: 5 are inherited framework members (`GameActivity.finish()` from
   `android.app.Activity`), and 4 are `GetFieldID` sites whose descriptor argument my dataflow
   could not resolve. No unexplained contradictions.
8. **Djinni helper recovery (`djinni.py`).** ~150 member lookups are *invisible* to step 4 because
   Roblox's Djinni (Snapchat fork) binding layer routes them through free functions
   (`jniFindClass(name)`, `jniGetMethodID(cls,name,sig)`, `jniGetFieldID(cls,name,type)`) that
   fetch the env from a thread-local *inside* the helper — so there is no `JNIEnv` deref at the
   call site. I identified the helpers by argument shape over all `BL` sites carrying candidate
   strings: `0x02258ff8`/`0x02258f78`/`0x02258d30` take a class name; `0x0225914c` (78 sites) takes
   `(name, descriptor)`; `0x0225921c` (48 sites) takes `(name, type)`; plus `0x055afec4` and
   `0x02335e34`. That recovered **151 more member lookups over 35 classes** (123 with a class
   attributed, 97 of which the dex confirms).
9. **`dexcode.py` — Java→native call edges.** A correct dex instruction-width table lets me walk
   every `code_item` and collect `invoke-*` method references. The scan independently reproduces
   706 native methods and yields the exact set of Java methods that call each native method — this
   is what §4 and §8 are built on. VERIFIED, not inferred.
10. **`plt.py` — PLT resolution.** `.rela.plt` (534 `R_AARCH64_JUMP_SLOT`) + `.got.plt` index →
    `.plt` entry address (`0x62d6010 + 32 + 16·i`) gives a name for every `bl 0x62d6xxx`. This is
    how the NDK calls in §6 were named in a stripped binary.

### 1.1 Where the method is weakest (stated honestly)

* The interpreter is **straight-line**, not a CFG walk. Conditional branches are ignored; I keep
  going past `ret` (LLVM lays continuation and cold blocks after early returns) while invalidating
  caller-saved registers and treating epilogue callee-save reloads as no-ops. This recovers sites
  that a stop-at-first-`ret` pass loses (e.g. `GetJavaVM` in `initializeNativeCode` sits 0x64 bytes
  *after* an early `ret`), at the cost of some register staleness. Because detection additionally
  requires a tainted-`JNIEnv` base, staleness produces misses far more often than false positives.
* **135 of 313** directly-detected member lookups could not have their class resolved by in-binary
  dataflow alone; 59 were then pinned by unique dex `(name,descriptor)` match, 21 remain
  ambiguous or unattributed (listed as `!unresolved` / `!ambiguous` in Section D).
* **Coverage is a lower bound.** The counts are "what I could prove", so the true surface is
  ≥ 409 members, not ≤. Any Roblox Java member reached only through a cached `jmethodID` in a
  global whose `GetMethodID` site my analysis missed would be absent. The dex gives an upper bound
  in the other direction: the union of all members of the 104 classes is far larger than 409, so
  the shim does not need to be complete per class — only per member actually looked up.

---

## 2. Q1 — Native → Java: the JNI functions and the names they look up

### 2.1 JNI functions actually used (VERIFIED, complete list in Section A1)

**59 distinct `JNINativeInterface` slots, 943 call sites.** Grouped:

| Group | Slots used |
|---|---|
| Class/member resolution | `FindClass`, `GetObjectClass`, `GetMethodID`, `GetStaticMethodID`, `GetFieldID`, `GetStaticFieldID` |
| References | `NewGlobalRef`, `DeleteGlobalRef`, `NewLocalRef`, `DeleteLocalRef`, `NewWeakGlobalRef`, `IsSameObject` |
| Exceptions | `Throw`, `ThrowNew`, `ExceptionOccurred`, `ExceptionDescribe`, `ExceptionClear`, `ExceptionCheck` |
| Instance calls (**all through the `…V` slots**) | `CallObjectMethodV`, `CallBooleanMethodV`, `CallIntMethodV`, `CallLongMethodV`, `CallFloatMethodV`, `CallDoubleMethodV`, `CallVoidMethodV`, `NewObjectV` |
| Static calls | `CallStaticObjectMethodV`, `CallStaticIntMethodV`, `CallStaticVoidMethodV` |
| Fields | `GetObjectField`, `GetBooleanField`, `GetIntField`, `GetLongField`, `GetFloatField`, `GetDoubleField`, `GetStaticObjectField`, `GetStaticIntField` |
| Strings | `NewString`, `GetStringLength`, `GetStringChars`, `ReleaseStringChars`, `NewStringUTF`, `GetStringUTFChars`, `ReleaseStringUTFChars` |
| Arrays | `GetArrayLength`, `NewObjectArray`, `GetObjectArrayElement`, `SetObjectArrayElement`, `NewLongArray`, `Get{Byte,Int,Float}ArrayElements`, `Release{Byte,Int,Float}ArrayElements`, `GetByteArrayRegion`, `SetLongArrayRegion` |
| VM / misc | `RegisterNatives`, `GetJavaVM`, `NewDirectByteBuffer` |

**Two findings here matter a lot for the shim's shape:**

* **Every `CallXxxMethod` in the engine funnels through the `…MethodV` slot.** There is exactly
  **one** call site for each of `CallObjectMethodV`, `CallBooleanMethodV`, `CallIntMethodV`,
  `CallLongMethodV`, `CallFloatMethodV`, `CallDoubleMethodV`, `CallVoidMethodV`,
  `CallStaticObjectMethodV`, `CallStaticVoidMethodV`, `NewObjectV` — and **zero** call sites for
  any non-`V` or `…A` variant. This is the C++ `jni.h` inline wrapper
  (`_JNIEnv::CallVoidMethod` does `va_start` then `functions->CallVoidMethodV(this,…)`), emitted
  once as a `linkonce_odr` out-of-line copy and ICF-merged by LLD; the whole engine reaches it by
  `bl`. **Omnidroid must implement the `…V` (`va_list`) variants correctly.** Implementing only the
  varargs forms and leaving the `…V` slots as stubs would fail everywhere.
* **170 of 233 slots are never dereferenced** (Section A3). Notably absent: `DefineClass`,
  `GetSuperclass`, `IsAssignableFrom`, `IsInstanceOf`, `AllocObject`, `NewObject`/`NewObjectA`,
  `PushLocalFrame`/`PopLocalFrame`, `EnsureLocalCapacity`, `MonitorEnter`/`MonitorExit`,
  `UnregisterNatives`, `GetObjectRefType`, `GetPrimitiveArrayCritical`, `GetStringCritical`,
  `GetDirectBufferAddress`/`Capacity`, `DeleteWeakGlobalRef`, every `FromReflected*`/`ToReflected*`,
  and `GetVersion`. `SetXxxField` (all 9) and `SetStaticXxxField` (all 9) are also never used —
  **the engine only ever reads Java fields, never writes them.**

### 2.2 JavaVM contract (VERIFIED)

Only **2 slots**, 5 sites:

* `GetEnv` (index 6, 0x30) — `0x2174de0`, `0x21750b0`, `0x21e3bc0`, and a 4th at `0x21e3f18`.
  At `0x2174dac` the version argument is materialised as `mov w2,#6; movk w2,#1,lsl#16` =
  **`0x00010006` = `JNI_VERSION_1_6`**.
* `AttachCurrentThread` (index 4, 0x20) — `0x2174e1c`, `0x21750ec`. Reached only when `GetEnv`
  returns `JNI_EDETACHED` (`cmn w0,#2` at `0x2174de4`); the code then calls `gettid@plt`
  (`0x62d64c0`), formats a thread name, and passes a `JavaVMAttachArgs` in `x2`.
  **So `AttachCurrentThread` must honour the thread-name field.**
* `DetachCurrentThread` and `DestroyJavaVM` are **never** called. `JNI_OnLoad` is at `0x2173ff4`
  and takes `JavaVM*` in `x0`; it must return `0x00010006` or better.

### 2.3 Java names referenced (VERIFIED, complete)

* **128 class-name literals** (Section B) — all in `.rodata`; a whole-binary sweep of every other
  section found none.
* **135 descriptor literals** (Section C).
* **64 `FindClass` sites** with a literal class name (direct), plus **58** more via the Djinni
  `jniFindClass` helpers.
* By package (the 128 class-name literals, VERIFIED counts):
  `com/roblox/protocols/*` **59** · `java/*` **18** · `com/roblox/engine/*` **12** ·
  `android/*` **10** · `com/roblox/client/*` **10** · `com/roblox/universalapp/*` **7** ·
  `com/google/androidgamesdk/*` **3** · `org/fmod/*` **3** · `androidx/*` **2** ·
  `com/roblox/{audio,platform}` **2** · `com/snapchat/djinni` **1** · `org/webrtc/*` **1**.

---

## 3. Q2 — Class-by-class requirement table

Full table with per-row evidence tags: **Section D** of the lists file. Summary by category:

| Category | Classes | Members |
|---|---:|---:|
| (a) core JDK (`java/*`) | 16 | 33 |
| (b) Android framework (`android/*`) | 9 | 66 |
| (b) AndroidX (present in this dex) | 2 | 13 |
| (c) AGDK GameActivity / GameTextInput | 3 | 14 |
| (d) app dex classes (Roblox, FMOD, WebRTC, Djinni) | 70 | 262 |
| unresolved / ambiguous | 4 | 21 |
| **total** | **104** | **409** |

### 3.1 Ranked by how load-bearing each is for startup

**Tier 0 — the process cannot start without these.**

| Class | Cat | Members needed | Why |
|---|---|---|---|
| `com/google/androidgamesdk/GameActivity` | c | `finish()V`, `setWindowFlags(II)V`, `getWindowInsets(I)Landroidx/core/graphics/Insets;`, `getWaterfallInsets()Landroidx/core/graphics/Insets;`, `setImeEditorInfoFields(III)V` | Resolved by the one-time init `0x285a614` that the exported `initializeNativeCode` trampoline calls **before** the real body. The `!gGameActivityClassInfo.finish` / `.setWindowFlags` / `.getWindowInsets` / `.getWaterfallInsets` / `.setImeEditorInfoFields` assertion strings (`.rodata` 0x522bad, 0x2ecd98, 0x2ecdbf, 0x297fc0, 0x323005) are `CHECK_NOT_NULL`-style aborts: a missing member is fatal. |
| `androidx/core/graphics/Insets` | b | fields `left`,`top`,`right`,`bottom` | Read at `0x285a7c0..0x285a850`, immediately after the `getWindowInsets` lookup. |
| `androidx/core/view/WindowInsetsCompat$Type` | b | 9 static `()I` methods (`statusBars`, `navigationBars`, `captionBar`, `displayCutout`, `ime`, `mandatorySystemGestures`, `systemGestures`, `systemBars`, `tappableElement`) | Same init block; used to build the inset query mask. |
| `android/content/res/Configuration` | b | **18 int fields** + `getLocales()Landroid/os/LocaleList;` | `initializeNativeCode`'s 8th argument. Read at `0x285c84c`. Fields: `mcc mnc orientation touchscreen keyboard keyboardHidden hardKeyboardHidden navigation navigationHidden screenLayout uiMode screenWidthDp screenHeightDp smallestScreenWidthDp densityDpi colorMode fontScale fontWeightAdjustment`. |
| `java/lang/String` | a | `getBytes(Ljava/lang/String;)[B` | Plus the implicit `NewStringUTF`/`GetStringUTFChars` machinery (84 + 27 sites). |
| `android/app/ActivityThread` | b | static `currentActivityThread()`, static `currentApplication()`, `getApplication()` | **The engine fetches the `Application` by itself via hidden framework API** rather than being handed a `Context`. Omnidroid must answer this or the engine has no `Context`. |
| `java/lang/ClassLoader` | a | `loadClass(Ljava/lang/String;)Ljava/lang/Class;`, `findClass(…)`, `getClassLoader()` | Used by `com/snapchat/djinni/NativeObjectManager.getClassLoader()` and by `RBX::Security::Android::Detail::JvmClassLoaderHelper` (mangled names at `.rodata` 0x6f8244/0x6f82b8) — the standard "cache the app ClassLoader so `FindClass` works on native threads" pattern. **Not** dynamic code loading. |

**Tier 1 — needed to get past engine bootstrap and produce a surface.**

| Class | Cat | Members |
|---|---|---|
| `com/roblox/client/startup/MainGameActivity` | d | `getNativeHelper()Lcom/roblox/client/startup/NativeHelper;`, `bootstrapTheApp()V`, `syncCookiesFromEngine()V`, `openWebActivity(SS)V`, `showLeaveAppPrompt()V`, static `getAppUpgradeKey()Ljava/lang/String;` |
| `com/roblox/client/startup/NativeHelper` | d | **23 `gameActivity_*` callbacks** — `onFlagsLoaded(Ljava/nio/ByteBuffer;)V`, `onFlagsFailed()V`, `onEngineInitialized()V`, `onAppReady(S)V`, `onGameLoaded(J)V`, `onScreenOrientationChanged(IZ)V`, `showKeyboard(JZ[BL…NativeTextBoxInfo;)V`, `hideKeyboard()V`, … (full list in Section D) |
| `com/roblox/engine/jni/NativeGLJavaInterface` | d | 27 members, nearly all `static` (`getDeviceStaticParams()`, `gameLoadedCallback(J)V`, `exitGameWithError(I)V`, `onAppBridgeNotification(SS)V`, `onDataModelNotificationCallback(SS)V`, `screenOrientationChanged(I)V`, `showKeyboard(...)`, `getWebViewUserAgent()V`, `getMobileAdvertisingId()V`, …) |
| `com/roblox/engine/jni/user/NativeUserJavaInterface` | d | 9 statics (`getUserId()J`, `getUsername()`, `getDisplayName()`, `getIsUnder13()Z`, `getMembershipType()I`, `getTheme()`, `getPlatformName()`, `getAlternateName()`, `getHasRobloxSubscription()Z`) |
| `com/roblox/engine/jni/locale/NativeLocaleJavaInterface` | d | 3 statics (`getLocale`, `getGameLocale`, `getRobloxLocale`) |
| `com/roblox/engine/jni/reporter/SessionReporterJavaInterface` | d | 6 statics (`getAppVersion`, `getFilesDir`, `getLastLoggedInUser(Id)`, `sendSessionReport(SS)V`, `setEventTrackingGoogleAnalytics(SSSJ)V`) |
| `com/roblox/engine/jni/model/ClientLocalFlags` | d | `<init>()V`, `add(SS)V`, `size()I` — **constructed from `JNI_OnLoad`'s callees** |
| `com/roblox/client/flags/NativeFlagsInitResult` | d | `<init>(I)V`, `addBoolean(SZZ)V` |
| `com/roblox/engine/jni/model/NativeTextBoxInfo` | d | `<init>(FFFFFZIIIIIIZZZ)V` |
| `com/roblox/universalapp/logging/LoggingProtocol` | d | static `getProcessTimestamp()J` — looked up **inside `JNI_OnLoad` itself** at `0x21740c0` |
| `android/util/DisplayMetrics`, `android/content/res/Resources`, `android/content/Context` | b | `density`, `widthPixels`, `heightPixels`, `xdpi`, `ydpi`; `getDisplayMetrics()`; `getResources()` |
| `android/os/LocaleList`, `java/util/Locale` | a/b | `size()I`, `get(I)Ljava/util/Locale;`; `getLanguage/getCountry/getScript/getVariant` |
| `java/util/HashMap`, `java/util/List` | a | `<init>()V`, `<init>(I)V`, `put`; `get(I)`, `toArray()` |
| `java/lang/{Boolean,Integer,Long,Float,Double}` | a | `booleanValue/intValue/longValue/floatValue/doubleValue`, `Long.<init>(J)V` — Djinni boxes optionals |

**Tier 2 — input and text (needed for an interactive frame, not the first frame).**
`android/view/MotionEvent` (22 methods), `android/view/KeyEvent` (12 methods),
`com/google/androidgamesdk/gametextinput/State` (5 fields + `<init>(Ljava/lang/String;IIII)V`),
`com/google/androidgamesdk/gametextinput/InputConnection` (`setState`, `restartInput`,
`setSoftKeyboardActive(ZI)V`).

Note on MotionEvent/KeyEvent: `apk-analysis.md` §4.4 observed that **no** `AMotionEvent_*`/
`AKeyEvent_*` NDK symbols are imported. That is consistent — AGDK's glue reads the event through
**JNI accessors on the Java objects** (`getPointerCount`, `getAxisValue(II)F`,
`getHistoricalAxisValue(III)F`, `getRawX(I)F`, …) and copies into
`GameActivityMotionEvent`/`GameActivityKeyEvent`. So Omnidroid must synthesise Java
`MotionEvent`/`KeyEvent` *objects* that answer ~34 getters, **or** short-circuit
`onTouchEventNative` (whose 15 scalar parameters already carry most of the data) and fill the glue
buffers directly.

**Tier 3 — lazily initialised, skippable for a first frame.**
The 26 `com/roblox/protocols/*/generated/*` Djinni bridge classes (109 members). Djinni resolves
these on first use of each protocol, inside a `JniClass<T>` singleton constructor. If Omnidroid
returns a valid dummy for them, nothing touches them until the corresponding feature is used.
Same for `com/roblox/client/purchase/IAPPurchaseManager` (7), `com/roblox/engine/jni/video/*` (12),
`com/roblox/engine/jni/model/BatteryStatus` (14 boxed fields), `org/fmod/*`, `org/webrtc/*`,
`com/roblox/client/widgets/RecentlyPlayedWidgetHandler`, `OtaConfigHandler`,
`AppRatingPromptHandler`, `SystemThemeProtocol`, `JNIAchievement`, `CookieProtocol`,
`FlagCacheUtils`, `JNIAppRestarter`, `JNIBaseUrlSetter`, `LocalStorageManager`,
`GmaSdkAvailability`, `ExperienceSession`, `NetworkUtils`, `MediaCodecInfoUtils`,
`MediaPickerProtocolV2`, `FacialAgeEstimationProtocol`, `DomainAllowListChecker$Callback`,
`MessageBus$a/$b/$c`, `memstorage/Connection`, `messagebus/Connection`, `ChannelRecord`,
`ApplicationExitInfoCpp`, `NativeVideoInterface$VideoDeviceId`.

**Tier X — referenced but ABSENT from this APK's dex (VERIFIED).**
`com/roblox/platform/util/DeviceUtils.getScreenPhysicalSizeInMillimeters(Landroid/content/Context;)Landroid/graphics/Point;`
and five `signalVideo*`/`signalFormatChanged`/`signalCameraDisconnected` methods have **no
declaring class anywhere in the 26,620 dex classes**. `libroblox.so` will `FindClass`/`GetMethodID`
them and get `NULL`. **Consequence for the shim: failed lookups must return `NULL` with a pending
`NoSuchMethodError`/`ClassNotFoundException` and let the caller recover — not abort.** Roblox's own
code clearly tolerates this today.

### 3.2 Things attributable only to the injected payload — EXCLUDED

* `com/roblox/gloop/Loader` (4 natives: `nativeStart`, `nativeResize`, `nativeContext`,
  `getDownloadUrl`) is bound by `libzstd-jni-1.5.7-6.so`'s `JNI_OnLoad` via `RegisterNatives`.
* Dex bytecode shows `MainGameActivity.onCreate` calling `Loader.get` then `Loader.startLib`
  **before** `GameActivity.onCreate` — VERIFIED from `classes2.dex`. This is the injection point.
  Omnidroid should simply not implement `com.roblox.gloop.*` and not call it.
* `com/github/luben/zstd/*` (85 + 14 + 10 + … natives) is in the stock APK as a library but its
  `.so` here is trojanised; not on the engine's JNI surface either way.
* `com/appsflyer/AppsFlyer2dXConversionCallback` (7 `RegisterNatives`-only natives) has **no
  matching `.so`** in the APK at all — dead code even in the stock build.

**No requirement in §3.1 comes from the injected payload.** Every Tier 0/1/2 item was traced to
`libroblox.so` or to AGDK/AndroidX/framework classes.

---

## 4. Q3 — Java → native entry points that matter for startup

Of the 706 dex natives, **662 are statically exported `Java_*` symbols** and **44 must be bound by
`RegisterNatives`** (my mangler checks both short and long forms; `apk-analysis.md` reported
657/49 using short names only — the 5-method delta is methods that exist only under the
overload-mangled long name). Per-class breakdown of the 44: `GameActivity` 23,
`AppsFlyer2dXConversionCallback` 7 (dead), `com.roblox.gloop.Loader` 4 (excluded),
`com.github.luben.zstd.Zstd` 3, `org.fmod.MediaCodec` 2, `NativeAppBridgeInterface` 1,
`NativeQuoteInterface` 1, `org.webrtc.Logging` 1, `WebRtcAudioManager` 1, `pk.z` 1.
Also: **52 exported `Java_*` symbols have no dex-declared native method** — dead exports
Omnidroid need not implement.

### 4.1 The 24 GameActivity natives — exact signatures (VERIFIED)

Recovered **not** from symbols but from the `JNINativeMethod[24]` array located in
`.data.rel.ro` at **`0x062dc1c8`** (24 × 24 bytes; each entry's three pointers are
`R_AARCH64_RELATIVE` relocations decoded out of the APS2 blob, pointing at a name string in
`.rodata`, a descriptor string in `.rodata`, and a function in `.text`). Full table with function
addresses: Section F. Signatures, in table order:

```
initializeNativeCode   (Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Landroid/content/res/AssetManager;[BLandroid/content/res/Configuration;)J
getDlError             ()Ljava/lang/String;
terminateNativeCode    (J)V
onStartNative          (J)V
onResumeNative         (J)V
onSaveInstanceStateNative (J)[B
onPauseNative          (J)V
onStopNative           (J)V
onConfigurationChangedNative (JLandroid/content/res/Configuration;)V
onTrimMemoryNative     (JI)V
onWindowFocusChangedNative (JZ)V
onSurfaceCreatedNative (JLandroid/view/Surface;)V
onSurfaceChangedNative (JLandroid/view/Surface;III)V
onSurfaceRedrawNeededNative (JLandroid/view/Surface;)V
onSurfaceDestroyedNative (J)V
onTouchEventNative     (JLandroid/view/MotionEvent;IIIIIJJIIIIIIFF)Z
onKeyDownNative        (JLandroid/view/KeyEvent;)Z
onKeyUpNative          (JLandroid/view/KeyEvent;)Z
onTextInputEventNative (JLcom/google/androidgamesdk/gametextinput/State;)V
onWindowInsetsChangedNative (J)V
setInputConnectionNative (JLcom/google/androidgamesdk/gametextinput/InputConnection;)V
onContentRectChangedNative (JIIII)V
onSoftwareKeyboardVisibilityChangedNative (JZ)V
onEditorActionNative   (JI)V
```

These are **exactly** the 24 natives declared by `Lcom/google/androidgamesdk/GameActivity;` in
`classes2.dex` — cross-checked method-for-method. `initializeNativeCode` appears in the table
*and* as a static export; the exported symbol `0x0285b6dc` is a trampoline that runs a one-time
class-info init (`bl 0x285a614`) and then tail-calls the registered implementation `0x0285b750`.

`onTouchEventNative`'s 15 scalar parameters (`pointerCount, historySize, deviceId, source, action,
eventTime, downTime, flags, metaState, actionButton, buttonState, classification, edgeFlags,
precisionX, precisionY`) plus `setInputConnectionNative` / `onEditorActionNative` /
`onSoftwareKeyboardVisibilityChangedNative` / `onWindowInsetsChangedNative` and the
`gametextinput/State` (rather than `Settings`) parameter type place this at **AGDK
`game-activity` 2.0.x or later**. See §5.3 for why an exact version cannot be pinned.

### 4.2 Verified Java→native call edges on the startup path

From dex bytecode (`dexcode.py`), in lifecycle order. Every arrow below is VERIFIED.

```
RobloxApplication.onCreate      -> JNIBaseUrlProtocol.init(Landroid/content/Context;)V
                                -> JNIWebLoginProtocol.init(Landroid/content/Context;)V
ActivitySplash.onCreate         -> NativeReportingInterface.initAppShellReporter()V
bh.x0.W0  (settings bootstrap)  -> NativeSettingsInterface.nativeInitFastLog()V
                                -> nativeSetRobloxVersion(S)V  nativeSetRobloxChannel(S)V
                                -> nativeSetBaseUrl(SS)V       nativeSetUserId(S)V
                                -> nativeOverrideChannelPlatformName(S)V / …2(S)V
                                -> nativeSetExceptionReasonFilename(S)V
bh.x0.T0                        -> nativeSetCacheDirectory(S)V  nativeSetFilesDirectory(S)V
bh.x0.X0                        -> nativeSetPlatformHeadersWithIdfa(SSS)V
bh.x0.S0 / U0                   -> nativeSetMultipleCookies(SS)V / nativeSetHttpClientProxy(SJ)V
MainGameActivity.b2             -> MainGameActivity.nativeSetAssetPath(S)V
MainGameActivity.a2             -> MainGameActivity.nativePreloadFlagOverrides(S)V
MainGameActivity.E2             -> NativeSettingsInterface.nativeSetDeviceInfo(L…DeviceParams;)V
                                -> MainGameActivity$Companion.e -> MainGameActivity.nativeAppBridgeSetInitParams(L…InitParams;)V
NativeHelper.Q                  -> NativeSettingsInterface.nativeSetExternalDirectory(S)V
                                -> NativeSettingsInterface.nativeSetPreferencesFile(S)V
                                -> NativeGLInterface.nativeSetAppPreviousExitReasons(Ljava/util/List;)V
MainGameActivity.onCreate       -> [Loader.get, Loader.startLib]   <-- INJECTED, EXCLUDED
                                -> GameActivity.onCreate  ==> initializeNativeCode(...)J
                                -> NativeHelper.n0(I, SurfaceView, RbxKeyboard, vk.e$f)
GameActivity.onCreate           -> GameActivity.getDlError()Ljava/lang/String;
                                -> GameActivity.initializeNativeCode(...)J
                                -> GameActivity.setInputConnectionNative(J, InputConnection)V
GameActivity.surfaceCreated     -> onSurfaceCreatedNative(J, Surface)V
GameActivity.surfaceChanged     -> onSurfaceChangedNative(J, Surface, III)V
GameActivity.onStart/onResume   -> onStartNative(J)V / onResumeNative(J)V
GameActivity.onWindowFocusChanged -> onWindowFocusChangedNative(JZ)V
GameActivity.onGlobalLayout     -> onContentRectChangedNative(JIIII)V
GameActivity.K                  -> onWindowInsetsChangedNative(J)V
fi.e$f.a                        -> NativeGLInterface.nativeInitClientSettings(SSS)I
fi.e$f.b                        -> NativeGLInterface.nativePostClientSettingsLoadedInitialization3(Ljava/util/List;)V
fi.e.E                          -> NativeGLInterface.nativeGameGlobalInit()V
fi.e.F                          -> NativeGLInterface.nativeAppBridgeV2StartAppWithParams(L…StartAppParams;)V
fi.e.H                          -> NativeGLInterface.nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams(Landroid/view/Surface;L…PlatformParams;)V
MainGameActivity$Companion.g    -> MainGameActivity.nativeRetryInit()V
vk.e.onTouch                    -> NativeInputInterface.nativePassInput(IFFIII)V / nativePassInputBatch([I[FIIII)V
vk.e.onSensorChanged            -> NativeInputInterface.nativePassAccelerometerChange(FFF)V / …Gyroscope / …Gravity
```

**Important structural fact (VERIFIED):** Roblox's `GameActivity` fork **extends `Lj/b;`**
(obfuscated `androidx.appcompat.app.AppCompatActivity`), not `android.app.Activity`, and Roblox
attaches its own `vk.e` touch/sensor listener to the SurfaceView, routing game input through
`NativeInputInterface.nativePassInput*` rather than AGDK's `onTouchEventNative`. Both input paths
exist in the binary; the Roblox one is the live one.

**~34 native methods** appear on the verified startup path above. That is the working target, not
706. Full list of all 706 with signatures, static/instance flag and binding mechanism: Section G.

---

## 5. Q4 — AGDK GameActivity specifics

### 5.1 Is it statically linked? — YES, VERIFIED

* The `JNINativeMethod[24]` array lives in **`libroblox.so`'s** `.data.rel.ro` at `0x062dc1c8`,
  and all 24 `fnPtr`s relocate into **`libroblox.so`'s** `.text` (`0x0285b750`–`0x0285c6a0`).
* `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode` is exported by
  **`libroblox.so`** (`apk-analysis.md` §5.1) and by no other library.
* The AGDK C++ sources' own diagnostic strings are in `libroblox.so`'s `.rodata`:
  `!gGameActivityClassInfo.finish` (0x522bad), `.setWindowFlags` (0x2ecd98),
  `.getWindowInsets` (0x2ecdbf), `.getWaterfallInsets` (0x297fc0),
  `.setImeEditorInfoFields` (0x323005), `"GameActivity"` log tag (0x441793),
  `GameActivity_register` (0x25e975), `"Unable to retrieve native ALooper"` (0x4b2cd0),
  `"could not create pipe: "` (0x3ce5d4).
* `android_native_app_glue` is linked in too: `"Failure writing android_app cmd: %s"` (0x45f5d0),
  `"android_app_set_activity_state timed out waiting for cmd %d"` (0x4b2cf2).
* `GameTextInput` is linked in: `"Warning: called GameTextInput_init twice without calling
  GameTextInput_destroy"` (0x593047), `"Can't find gametextinput.State constructor"` (0x53fc77).
* There is **no separate `.so`** — the APK's other 10 libraries contain none of this.

### 5.2 The concrete native-side contract

Full detail in **Section K** of the lists file. Summary:

`initializeNativeCode` (impl at `0x0285b750`) is the constructor. It:

1. `operator new(0x278)` → a **632-byte `NativeCode`** whose first 0x50 bytes are the public
   `GameActivity` struct. Zeroes it.
2. `__system_property_get("ro.build.version.sdk")` → `activity->sdkVersion` (**+0x30**).
3. `ALooper_forThread()` + `ALooper_acquire()` → `looper` (+0x158). If NULL → logs
   `"Unable to retrieve native ALooper"` and **returns 0** (failure).
4. `pipe()` + `fcntl(F_SETFL, O_NONBLOCK)` ×2 → `msgread`/`msgwrite` (+0x150/+0x154).
5. `ALooper_addFd(looper, msgread, ident=0, ALOOPER_EVENT_INPUT=1, callback=0x285d57c, data=this)`.
6. `activity->callbacks = this + 0x50` (**+0x00**).
7. `env->GetJavaVM(&activity->vm)` (**+0x08**); `activity->env = env` (**+0x10**).
   On failure logs `"GameActivity GetJavaVM failed"` and aborts construction.
8. `activity->javaGameActivity = env->NewGlobalRef(thiz)` (**+0x18**).
9. `GetStringUTFChars` on the three path arguments → std::strings at +0xf8/+0x110/+0x128, with
   `internalDataPath` (**+0x20**), `externalDataPath` (**+0x28**), `obbPath` (**+0x48**) pointing
   into them; `ReleaseStringUTFChars` after each.
10. `NewGlobalRef(jAssetMgr)` (+0x160) and `AAssetManager_fromJava(env, jAssetMgr)` →
    `activity->assetManager` (**+0x40**).
11. Reads all 18 `Configuration` int fields + `getLocales()` (helper `0x285c84c`).
12. `GetByteArrayElements` / `GetArrayLength` on `savedState`, then calls
    **`GameActivity_onCreate(activity, savedState, savedStateSize)`** at `0x0285e7c8`, then
    `ReleaseByteArrayElements`.
13. `GameTextInput_init(env, 0)` → +0x168, then
    `GameTextInput_setEventCallback(gameTextInput, callbacks->onTextInputEvent, this)`.
14. **Returns the `NativeCode*` as the `jlong` handle.** Java stores it and passes it back as the
    first argument of all 23 other natives.

`GameActivity_onCreate` (`0x0285e7c8`, the app glue, **not** Roblox code) then:

1. Fills **all 21 `GameActivityCallbacks` slots** (stores at `0x285e814`–`0x285e8cc` into
   `activity->callbacks + 0x00..0xa0`). **Roblox registers nothing itself** — the glue owns every
   callback and translates them into `APP_CMD_*` messages on the pipe.
2. `operator new(0x180)` → a **384-byte `android_app`**; `app->activity = activity` (+0x10);
   `pthread_mutex_init(&app->mutex)` (+0xc8); `pthread_cond_init(&app->cond)` (+0xf0);
   copies `savedState`; `pipe()` (+0x120/+0x124).
3. `pthread_attr_init` + `pthread_attr_setdetachstate(PTHREAD_CREATE_DETACHED)` +
   `pthread_create(&app->thread, &attr, android_app_entry, app)`.
4. `pthread_mutex_lock` → `while (!app->running) pthread_cond_wait` → `pthread_mutex_unlock`.
   **`initializeNativeCode` blocks here until the game thread signals.**
5. `activity->instance = app` (**+0x38**).

`android_app_entry` (`0x0285f8d8`, game thread):
`AConfiguration_new` → `AConfiguration_fromAssetManager(config, activity->assetManager)` →
`AConfiguration_getLanguage/getCountry`; allocates the glue's input ring buffers
(`0x6e00`-byte motion buffers ×2, capacity 16; `0x100`-byte key buffers ×2, capacity 4);
`ALooper_prepare(ALOOPER_PREPARE_ALLOW_NON_CALLBACKS=1)`;
`ALooper_addFd(looper, msgread, LOOPER_ID_MAIN=1, ALOOPER_EVENT_INPUT=1, NULL, &app->cmdPollSource)`;
`app->running = 1` + `pthread_cond_broadcast`; then calls
**`android_main(app)` at `0x02bcc6a4`** — Roblox's entry point (304 bytes):

```
android_main(android_app* app):
    log "[FLog::NativeMain] [android_main] Create a new NativeEngine:"
    assert(nativeEngine_ == nullptr)                 // else __android_log_print + "*** Roblox-App ABORTING." + store to nullptr
    ne = operator new(0x328)                          // 808-byte NativeEngine
    NativeEngine::NativeEngine(ne, app)   @0x02bcd02c
    nativeEngine_ = ne                                // global 0x0683d888
    NativeEngine::GameLoop()              @0x02bcd5d0
    nativeEngine_ = nullptr
```

Neither `GameActivity_onCreate` nor `android_main` is an exported symbol — both are statically
linked, so Omnidroid **cannot** call them directly; it must go through
`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`.

### 5.3 AGDK version — cannot be pinned exactly; bounded instead

* There is **no** `META-INF/*games-activity*.version` file (I enumerated all 100+ `.version`
  files), no `androidx.games` marker, no `gamesdk` version string in `.rodata`. Roblox vendored
  and modified the AGDK source: the dex `GameActivity` extends `Lj/b;` (AppCompatActivity) instead
  of `android.app.Activity`, and its `mNativeHandle`/singleton fields are `static`.
* **Lower bound (INFERRED, well supported):** `game-activity` **2.0.0 or later**, because
  `setInputConnectionNative`, `onEditorActionNative`, `onSoftwareKeyboardVisibilityChangedNative`
  and `onWindowInsetsChangedNative` all exist, the text-input parameter type is
  `gametextinput/State`, and `onTouchEventNative` takes the 15 expanded scalar parameters rather
  than reading them from the `MotionEvent` in native code.
* The 21-slot `GameActivityCallbacks` and 24-entry native table are the authoritative contract
  regardless of upstream version number — use Section K, not a guessed release.

---

## 6. Q5 — Does anything force real dex execution?

Every mechanism named in the question was searched for explicitly. **All negative** (Section M):

| Mechanism | Result |
|---|---|
| `dalvik/system/*` (`DexClassLoader`, `InMemoryDexClassLoader`, `PathClassLoader`, `BaseDexClassLoader`) | **0** string hits anywhere in `libroblox.so` |
| `java/lang/reflect/*`, `Class.forName`, `getDeclaredMethod`, `getDeclaredField`, `defineClass` | **0** hits |
| `JNIEnv::DefineClass` (slot 0x28) | **never dereferenced** |
| `FromReflectedMethod` / `ToReflectedMethod` / `…Field` | **never dereferenced** |
| `java/lang/invoke/*` (MethodHandles) | **0** hits |
| Java-side HTTP (`java/net/URL`, `HttpURLConnection`, `okhttp3/*`) | **0** hits. Networking is native: BSD sockets + `getaddrinfo` in the import table (`apk-analysis.md` §4.5); the only Java touch is `nativeSetHttpClientProxy` / `nativeSetMultipleCookies` / `nativeGetCookiesInNetscapeFormat` feeding **into** native code |
| Java-side file I/O (`java/io/File`, `FileInputStream`) | **0** hits. Assets go through `AAssetManager_*` NDK calls; paths arrive as strings |
| `android/webkit/*` | **0** hits. Login/webview is a Java-side concern reached *from* native via `MainGameActivity.openWebActivity(SS)V` — a single downcall the host can implement natively or stub |
| `android/database/sqlite/*` | **0** hits. `memstorage/MemStorage` has 6 natives and its Java side is a 2-member wrapper (`Connection.<init>(J)V` + field `ref`) — the storage is native |
| `RegisterNatives` from Java static initialisers | Only **2** `RegisterNatives` sites in the whole binary. Both are in `libroblox.so`'s own `JNI_OnLoad`/`GameActivity_register` path, i.e. native-driven, not Java-driven |
| `System.loadLibrary` from Java | Exists in `GameActivity.onCreate`, but Omnidroid `dlopen`s the library itself, so this is not a dex requirement |

**The one reflective-looking thing is benign.** `java/lang/ClassLoader.loadClass` /
`findClass` are looked up, reached from `com/snapchat/djinni/NativeObjectManager.getClassLoader()`
and from `RBX::Security::Android::Detail::JvmClassLoaderHelper`. That is the canonical
"cache the app `ClassLoader` in `JNI_OnLoad` so `FindClass` resolves app classes on threads
attached later" pattern. Omnidroid implements it as: one fake `ClassLoader` object whose
`loadClass(String)` returns the same `jclass` the native `FindClass` would. Roughly 20 lines.

### 6.1 What *is* unavoidable, and how to scope it

Nothing requires interpreting dex, but three things require **native reimplementation of Java
behaviour**, which is a different cost:

1. **The `NativeHelper` / `fi.e` / `bh.x0` orchestration (INFERRED but strongly evidenced).**
   `libroblox.so` does not bootstrap itself. Flags, client settings, base URLs, directories,
   device params and `InitParams` all arrive **from Java**, and `NativeEngine` waits for them.
   Omnidroid must write a native "shell" that performs the §4.2 downcall sequence in order. Scope:
   ~34 native calls plus the ~46 Java members the engine calls back into
   (`NativeHelper.gameActivity_*`, `NativeGLJavaInterface`, `NativeUserJavaInterface`,
   `NativeLocaleJavaInterface`, `SessionReporterJavaInterface`). Every one of those is a getter or
   a notification sink — the host defines the answers. This is the real work, and it is
   **smaller than a dex interpreter**: a correct dex interpreter needs the full 26,620-class
   dependency closure (AndroidX lifecycle, Kotlin coroutines, OkHttp, Dagger, Play Services) to
   run `MainGameActivity.onCreate` at all. `NativeHelper.n0` alone takes
   `(I, android/view/SurfaceView, com/roblox/client/RbxKeyboard, vk/e$f)`.
2. **`android.app.ActivityThread.currentApplication()`** — the engine self-serves a `Context`.
   The shim must return an object that answers `getResources()` → `getDisplayMetrics()` →
   5 fields. ~10 Java members total.
3. **`MotionEvent` / `KeyEvent` objects** — 34 getters, if the AGDK input path is used at all.
   Avoidable for a first frame.

**Verdict: a dex interpreter is not required and would be strictly more expensive.**
Recommended architecture: a native `JNIEnv`/`JavaVM` with a flat `jclass`/`jmethodID`/`jfieldID`
registry keyed by `(class, name, descriptor)` strings, native "class" objects implemented as C++
structs with function pointers, a local/global ref table, and a host-side startup script that
issues the §8 sequence. **~409 distinct Java members**, realistically **~120 of which are needed to
reach a first frame** (Tier 0 + Tier 1 + the ~34 downcalls).

---

## 7. Injected-payload attributions, isolated for exclusion

For completeness and to keep them out of the required surface:

* `com/roblox/gloop/Loader` — 4 natives, bound by `libzstd-jni-1.5.7-6.so`. Called from
  `MainGameActivity.onCreate` (`Loader.get`, `Loader.startLib`) before `GameActivity.onCreate`.
  **Exclude.** No Roblox JNI lookup in `libroblox.so` references it.
* `com/github/luben/zstd/*` — 85 + 14 + 10 + 9 + 7 + 6 + 5 + 5 + 3 + 3 natives in the dex; none is
  referenced from `libroblox.so`'s JNI surface. Out of scope for the shim.
* Two 1-entry `JNINativeMethod` tables at `.data.rel.ro` `0x0667e478` and `0x0667edd8` register
  `nativeCacheAudioParameters (IIIZZZZZZZIIJ)V` (fns `0x04dd1124`, `0x04dd8294`). These are
  **stock** WebRTC (`org/webrtc/voiceengine/WebRtcAudioManager`) inside `libroblox.so`, not
  injected.
* `android.permission.MANAGE_EXTERNAL_STORAGE` and `assets/gloop/` are payload artefacts —
  no JNI consequence.

---

## 8. Q6 — Ordered minimum viable startup contract

What Omnidroid must provide, in call order, from `dlopen` to "engine asks for a rendering surface
and a Vulkan/GLES instance". **V** = VERIFIED, **I** = INFERRED.

| # | Step | Omnidroid must provide | Ev. |
|---:|---|---|---|
| 1 | Inflate `lib/arm64-v8a/*.so` to files (all DEFLATED, 4-byte aligned → cannot be mmap-ed from the APK) | a filesystem view of `<app>/lib/arm64/` | **V** (`apk-analysis.md` §1.3) |
| 2 | Map `libroblox.so`, apply **568,272 APS2-packed** relocations + 534 `JUMP_SLOT` | APS2 decoder; `DF_BIND_NOW` semantics | **V** |
| 3 | Satisfy `DT_NEEDED`: `libOpenMAXAL`, `libmediandk`, `libandroid`, `libm`, `libOpenSLES`, `libGLESv2`, `libEGL`, `liblog`, `libdl`, `libc` — 565 undefined symbols | bionic-shaped libc + the 32 `libandroid` NDK entry points | **V** |
| 4 | **Program `TPIDR_EL0` for every guest thread with a bionic TLS block; slot 5 (`+0x28`) = stack guard** | per-thread TLS area | **V** (1,276 of 1,282 `MRS` sites read `+0x28`) |
| 5 | Run the **3,594 `DT_INIT_ARRAY`** entries | working `__cxa_atexit`, `malloc`, `pthread_key_*`, `__stack_chk_guard` | **V** |
| 6 | Call `JNI_OnLoad(JavaVM*, void*)` @`0x2173ff4`, expect `0x00010006` | `JavaVM` whose vtable has **`GetEnv`(0x30) and `AttachCurrentThread`(0x20)** at minimum | **V** |
| 6a | `JNI_OnLoad` caches the `JavaVM*` in the global at `0x07275550` (helper `0x1db2cf0`), then the scoped-attach helper `0x2174c04` does `ldar` on that global → `vm->GetEnv(&env, 0x00010006)`; on `JNI_EDETACHED` (`cmn w0,#2`) it attaches via `0x2173f48` and records "must detach". It yields a `{bool attached; JNIEnv* env;}` pair. Then `FindClass("com/roblox/universalapp/logging/LoggingProtocol")` + `NewGlobalRef` + `GetStaticMethodID("getProcessTimestamp","()J")` + `ExceptionCheck` | `FindClass`, `NewGlobalRef`, `GetStaticMethodID`, `ExceptionCheck`, `ExceptionClear` | **V** (pcs `0x2174028`, `0x2174c28`–`0x2174c44`, `0x2174074`, `0x2174090`, `0x21740c0`, `0x2174108`, `0x2174138`) |
| 6b | Three more registration helpers run with `x0 = JavaVM*` (`0x2174c90`, `0x2174e58`, `0x2175128`) and batch-resolve `NativeGLJavaInterface`, `NativeUserJavaInterface`, `NativeLocaleJavaInterface`, `SessionReporterJavaInterface`, `ClientLocalFlags`, … (161 `GetMethodID` + 84 `GetStaticMethodID` + 69 `GetFieldID` sites total) | the Tier 0/Tier 1 classes of §3.1, resolvable **now** | **V** |
| 7 | Simulate `RobloxApplication.onCreate`: `JNIBaseUrlProtocol.init(Context)`, `JNIWebLoginProtocol.init(Context)` | a `Context`-shaped object | **V** |
| 8 | Simulate `ActivitySplash.onCreate`: `NativeReportingInterface.initAppShellReporter()` | — | **V** |
| 9 | Settings bootstrap (`bh.x0`): `nativeInitFastLog`, `nativeSetRobloxVersion("2.738.1397")`, `nativeSetRobloxChannel`, `nativeSetBaseUrl`, `nativeSetCacheDirectory`, `nativeSetFilesDirectory`, `nativeSetExceptionReasonFilename`, `nativeSetPlatformHeadersWithIdfa`, `nativeSetUserId` | 11 downcalls, all `(String…)V` | **V** |
| 10 | `MainGameActivity.nativeSetAssetPath(String)`, `nativePreloadFlagOverrides(String)` | 2 downcalls | **V** |
| 11 | `NativeSettingsInterface.nativeSetDeviceInfo(DeviceParams)`, `nativeSetExternalDirectory`, `nativeSetPreferencesFile`; `NativeGLInterface.nativeSetAppPreviousExitReasons(List)` | a `DeviceParams` object; `java/util/List` with `get(I)`/`toArray()` | **V** |
| 12 | `MainGameActivity.nativeAppBridgeSetInitParams(InitParams)` (built by `MainGameActivity.E2` → `Companion.e`) | an `InitParams` object (`platformParams`, `deviceParams`, `baseURL`, `userAgent`, `isTablet`, `isPotato`, `isVrDevice` — verified from `E2`'s builder calls) | **V** |
| 13 | Call the **exported** `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode(env, thiz, internalDataDir, obbDir, externalDataDir, assetMgr, savedState, configuration)` | 3 `jstring`s, a Java `AssetManager` object, `NULL` byte array, a `Configuration` object with 18 int fields + `getLocales()`. Before this returns, the shim must answer `GameActivity.{finish,setWindowFlags,getWindowInsets,getWaterfallInsets,setImeEditorInfoFields}`, `Insets.{left,top,right,bottom}` and the 9 `WindowInsetsCompat$Type` statics — those are `CHECK_NOT_NULL` aborts | **V** |
| 13a | Host must also provide: `ALooper_forThread`/`_acquire`/`_addFd` returning a real looper (else the call **returns 0 and startup dies**), `pipe`, `fcntl(F_SETFL,O_NONBLOCK)`, `AAssetManager_fromJava`, `__system_property_get("ro.build.version.sdk")`, `env->GetJavaVM` | NDK + libc | **V** |
| 13b | It returns a **`jlong` = `NativeCode*`**. Keep it; it is argument 1 of all 23 other natives | — | **V** |
| 14 | Inside 13, the glue spawns the game thread (`pthread_create` detached, `android_app_entry`) and **blocks on `pthread_cond_wait` until `app->running`** | working `pthread_create`/`mutex`/`cond`, plus `AConfiguration_new`/`_fromAssetManager`/`_getLanguage`/`_getCountry` and a second `ALooper_prepare`/`ALooper_addFd` on the new thread | **V** |
| 15 | The game thread enters `android_main` → `new NativeEngine(app)` (808 B) → `NativeEngine::GameLoop()` | — | **V** |
| 16 | `GameActivity.setInputConnectionNative(handle, InputConnection)` (called from `GameActivity.onCreate` right after step 13) | an `InputConnection` object answering `setState`, `restartInput`, `setSoftKeyboardActive(ZI)V`; `State` with 5 fields + `<init>(Ljava/lang/String;IIII)V` | **V** |
| 17 | `onSurfaceCreatedNative(handle, Surface)` — releases any old `ANativeWindow`, calls `ANativeWindow_fromSurface(env, surface)`, stores at `NativeCode+0x140`, then `callbacks[7] = onNativeWindowCreated(activity, window)` → glue posts `APP_CMD_INIT_WINDOW` down the pipe | a Java `Surface` object that `ANativeWindow_fromSurface` accepts, i.e. Omnidroid's own `ANativeWindow` implementation | **V** |
| 18 | `onSurfaceChangedNative(handle, Surface, format, w, h)` — may call `callbacks[7]`, `callbacks[8] onNativeWindowResized`, `callbacks[10] onNativeWindowDestroyed` | `ANativeWindow_{getWidth,getHeight,getFormat,setBuffersGeometry,lock,unlockAndPost,acquire,release}` | **V** |
| 19 | `onStartNative(handle)` → `callbacks[0]`; `onResumeNative(handle)` → `callbacks[1]` | — | **V** |
| 20 | `onWindowFocusChangedNative(handle, true)` → `callbacks[6]`; `onContentRectChangedNative(handle,l,t,r,b)` → `callbacks[18]`; `onWindowInsetsChangedNative(handle)` → `callbacks[17]` | — | **V** |
| 21 | Client-settings / flags phase (`fi.e$f`): `NativeGLInterface.nativeInitClientSettings(S,S,S)I`, then `nativePostClientSettingsLoadedInitialization3(List)`. Engine answers with `NativeHelper.gameActivity_onFlagsLoaded(ByteBuffer)` (via `NewDirectByteBuffer`) or `gameActivity_onFlagsFailed()` | a `NativeFlagsInitResult` (`<init>(I)V`, `addBoolean(SZZ)V`), and a `NativeHelper` object | **V** |
| 22 | `NativeGLInterface.nativeGameGlobalInit()`, then `nativeAppBridgeV2StartAppWithParams(StartAppParams)` | `StartAppParams` object | **V** |
| 23 | Engine calls back `NativeHelper.gameActivity_onEngineInitialized()`, `gameActivity_onAppReady(String)`, `gameActivity_onScreenOrientationChanged(IZ)`, `NativeGLJavaInterface.getDeviceStaticParams()` | the ~46 callback members of Tier 1 | **V** (lookup sites at `0x2bd8f54`–`0x2bdae30`) |
| 24 | If the surface is re-created, `NativeGLInterface.nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams(Surface, PlatformParams)`; `PlatformParams` is read with `surface()Landroid/view/Surface;` + `platformParams()L…PlatformParams;` accessors at `0x258b1f0`/`0x2bcc9c4` | `PlatformParams` object | **V** |
| 25 | `NativeEngine::GameLoop()` drives `ALooper_pollOnce`, drains `APP_CMD_*` and initialises graphics: EGL first (`eglGetDisplay`…`eglCreateWindowSurface`, 17 hard-linked symbols) and Vulkan by `dlopen("libvulkan.so")` + `vkGetInstanceProcAddr` | EGL 1.4 + GLES 3.x; `libvulkan.so` loadable via `dlopen`/`dlsym` (0 `vk*` imports) | **V** (`apk-analysis.md` §7); the *ordering inside* `GameLoop` is **I** |
| 26 | Throughout: input via `NativeInputInterface.nativePassInput(IFFIII)` / `nativePassInputBatch([I[FIIII)` from the host's pointer events (Roblox's own path), **not** necessarily `onTouchEventNative` | — | **V** |

**Shutdown, for completeness:** `onPauseNative`→`callbacks[3]`, `onStopNative`→`callbacks[4]`,
`onSurfaceDestroyedNative`→`callbacks[10]`, `terminateNativeCode(handle)` → teardown helper
`0x285c6bc` → `operator delete`. `onSaveInstanceStateNative` → `callbacks[2]` returns a `jbyteArray`.

### 8.1 Failure modes to expect, in order

1. **Step 4** — wrong `TPIDR_EL0` layout: every prologue mismatches, `__stack_chk_fail` immediately.
2. **Step 5** — any of 3,594 static constructors touching an unimplemented libc call aborts before
   one line of Roblox code runs.
3. **Step 6a** — `FindClass` returning `NULL` where the code does not check: the `getProcessTimestamp`
   path in `JNI_OnLoad` does `ExceptionCheck` immediately after, so it is defensive; the
   `gGameActivityClassInfo` lookups in step 13 are **not** — they abort.
4. **Step 13a** — `ALooper_forThread()` returning `NULL` makes `initializeNativeCode` return `0`
   and Java-side startup fails silently. Provide a real looper on the "UI" thread *before* calling it.
5. **Step 14** — the cond-wait means a deadlock here is indistinguishable from a hang; instrument it.
6. **Steps 21–22** — the engine will sit waiting for flags/settings forever if the host never
   performs the `fi.e` sequence. This is the step most likely to be mistaken for "the engine is broken".

---

## 9. Reproduction

Scratchpad: `…\2692e040-…\scratchpad\jni\`.
`jnitab.py` (jni.h table) · `plt.py` (PLT→symbol) · `step1/2/3.py` (strings, `JNINativeMethod`
tables, `.eh_frame_hdr` function starts) · `xref.py` (numpy `ADRP`+`ADD` xrefs) ·
`cs_interp.py` + `run2.py` (capstone symbolic interp + env taint) · `resolve.py`/`resolve2.py`
(dex attribution) · `djinni.py` (helper-mediated lookups) · `dexmeth.py`/`dexcode.py`
(dex metadata + bytecode invoke scan) · `natives.py` (JNI name mangling ↔ exports) ·
`genlists.py`/`genlists2.py` (report lists) · `dump.py` (annotated disassembly).
Nothing outside `docs/research/jni-surface.md` and `docs/research/jni-surface-lists.txt` was
written into the project.
