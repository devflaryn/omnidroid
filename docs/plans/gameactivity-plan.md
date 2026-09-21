# Plan: GameActivity and step 13 (milestone M5)

Spec: `docs/research/jni-surface.md` §8 rows 13, 13a, 13b, 14 and **§5.2**, which decodes
`initializeNativeCode` (`0x0285b750`) instruction group by instruction group. Rationale:
`docs/DECISIONS.md` D7 (no JVM), D19 (the zero-dependency split), D22–D25 (what `omni-platform` may
grow), D28 (JNI without a JVM). Builds on M4 (`bionic-threads`), where `JNI_OnLoad` returns
`0x00010006` on the real engine and 19 of 21 scripted downcalls return.

Scope: reach **M5** — `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode` returns a
**non-zero `jlong`** on the real `libroblox.so`, which means the game thread was created, ran
`android_app_entry`, set `app->running` and broadcast, and the blocked `pthread_cond_wait` in the
calling thread woke. Excludes graphics entirely (M6) and the flags/settings orchestration of §8
rows 21–24 (M5/M6 boundary, taken after 13 returns).

The **Global Constraints** of `android-abi-plan.md` carry forward unchanged, and `docs/VERIFICATION.md`
carries forward as the list of ways a green suite here has proved less than it claimed. Two of its
entries bear directly on this milestone and are restated where they bite, below.

---

## What §5.2 says step 13 does, as an ordered list of things that must exist

The host cannot call `GameActivity_onCreate` or `android_main` — neither is exported (§5.3) — so
everything below happens *inside* one guest call and either all of it works or the call returns `0`.
Numbered as §5.2 numbers them:

| §5.2 | what the guest does | what must exist |
|---:|---|---|
| 1 | `operator new(0x278)`, zeroed | nothing new |
| 2 | `__system_property_get("ro.build.version.sdk")` | **bound already** — but the host must *set* the property |
| 3 | `ALooper_forThread()` + `ALooper_acquire()`; **null → logs and returns 0** | `ALooper`, on the *calling* thread, created before the call |
| 4 | `pipe()` + `fcntl(F_SETFL, O_NONBLOCK)` ×2 | `pipe`, `fcntl` — neither bound, neither among the 188 |
| 5 | `ALooper_addFd(looper, msgread, 0, ALOOPER_EVENT_INPUT, callback, this)` | `ALooper_addFd`, storing a **guest** callback |
| 6-8 | `env->GetJavaVM`, `NewGlobalRef(thiz)` | done (M4) |
| 9 | `GetStringUTFChars` ×3 + release | done (M4) |
| 10 | `NewGlobalRef(jAssetMgr)`, `AAssetManager_fromJava` | a Java `AssetManager` object **and** `AAssetManager_fromJava` |
| 11 | 18 `Configuration` int fields + `getLocales()` | declared (M4) — `fontScale`'s descriptor is the open contradiction |
| 12 | `GetByteArrayElements`/`GetArrayLength`, then **`GameActivity_onCreate`** | a null `savedState` is legal; the array family is M4's |
| 13 | `GameTextInput_init` | statically linked, not an import |
| 14 | returns the `NativeCode*` | — |

and `GameActivity_onCreate` then does the part that can hang:

| §5.2 | what the glue does | what must exist |
|---|---|---|
| 1 | fills all 21 `GameActivityCallbacks` slots | nothing — the glue owns them |
| 2 | `operator new(0x180)`, mutex/cond init, **a second `pipe()`** | `pipe` again |
| 3 | `pthread_attr_setdetachstate` + `pthread_create(android_app_entry)` | **done (D24)** |
| 4 | `while (!app->running) pthread_cond_wait` | **done (D24)** — and §8.1's fifth failure mode |
| 5 | `activity->instance = app` | — |

and `android_app_entry`, on the new thread:

`AConfiguration_new` → `_fromAssetManager` → `_getLanguage`/`_getCountry`; allocate the input ring
buffers; **`ALooper_prepare(1)`**; `ALooper_addFd(looper, msgread, LOOPER_ID_MAIN=1,
ALOOPER_EVENT_INPUT=1, NULL, &app->cmdPollSource)`; `app->running = 1`; `pthread_cond_broadcast`;
`android_main(app)`.

**So the milestone's critical path is: a pipe, a looper on two threads, an asset manager, and a
configuration.** `ANativeWindow` is step 17, not step 13 — but §8's row 13 note says the type has to
exist before the glue can store one, so it is scoped here as a **type and a refusal**, not as a
working window.

---

## Task 1: `pipe` and `fcntl` — and the descriptor space stops being closed

`omni-android/src/bionic/net.rs` states the argument `poll` and `select` rest on as a closed
three-step proof, and step 1 of it is *the only symbols that produce a descriptor are `open`,
`__open_2`, `opendir` and `fileno`; `pipe` is not in the 188 at all*. It is asserted mechanically by
`the_descriptor_space_poll_answers_over_is_closed`, which D25 wrote down as a **detector for this
exact moment**: "the day a phase binds `socket` for real, that test fails and this module has to grow
a real readiness source with it."

This task is that day. The test must **fail and be replaced**, not updated in place.

### Where a pipe lives

In `omni-platform::fs`, as a fourth `Entry` kind beside `File`, `Directory`, `Standard` and `Device`,
because `poll` and `select` observe one descriptor table and a pipe that lived anywhere else would
need a second descriptor namespace — two allocators that can hand out the same number.

**It needs no operating system.** A pipe here is an in-process byte queue with two ends; the bytes
never leave the process, because both ends belong to the same guest. So per D22's other half, it gets
**no fabricated `unsupported` arm** for Linux or macOS — that would assert that a queue this process
can hold cannot be held. This is the fourth phase running whose OS-surface prediction was too high,
and the plan says so in advance this time.

### What it must get right

* **Capacity is observable.** Linux's default pipe capacity is 65,536 bytes and a write past it
  blocks (or returns `EAGAIN` when `O_NONBLOCK`). A capacity that differs is not a wrong *answer*
  until something fills it; the glue writes 1-byte commands, so nothing on this path does. The number
  is stated as a policy constant with its Linux provenance, not derived.
* **A write to a pipe with no reader is `EPIPE` + `SIGPIPE`.** There is no signal delivery here
  (D24), so this is a **refusal by name**, not a fabricated `-1`: the guest would be entitled to a
  signal it cannot receive.
* **Partial writes.** POSIX guarantees atomicity only up to `PIPE_BUF` (4,096). A blocking write of
  more may be split; a non-blocking write of more than the free space writes what fits and returns
  that count. Getting this backwards — returning `EAGAIN` when some bytes would fit — is the kind of
  plausible answer this project refuses.
* **Readiness is the point.** `readable` iff the queue is non-empty **or every write end is closed**
  (that second half is what makes a reader see EOF rather than block forever). `writable` iff a read
  end is open and the queue has room.
* **Blocking reads must not hold the table lock**, and must be bounded the way `poll` already is:
  `MAX_SLEEP_SECONDS`, with an unbounded wait on an empty pipe refused by name. D16's runaway-guest
  defence is built from step budgets a sleeping thread does not consume.

### `fcntl`

Variadic (`int fcntl(int, int, ...)`), and among the 13 variadic imports Task 2's reviewer listed.
Only `F_GETFL` and `F_SETFL` are implemented, and `F_SETFL` accepts only `O_NONBLOCK` and
`O_APPEND`-free zero; every other command **refuses by name with the command number in the message**,
which is the measurement that says what the engine actually asks for.

### Tests

* A pipe round-trips bytes, in order, across a real guest `write` then `read` through translated
  ARM64 code — not through the host API.
* An empty non-blocking pipe reads `EAGAIN`; a full one writes `EAGAIN`.
* Every write end closed makes `read` return `0` and keeps returning `0`.
* `poll` on a pipe answers `POLLIN` only when there is something to read — **the assertion that a
  count cannot make**: assert the `revents` of the named entry, not the returned ready count.
* The closedness test is **replaced** by one asserting every descriptor kind has a readiness rule
  this seam implements — a set difference over the `Entry` variants, so a fifth kind added later
  fails it.
* Mutation rows in both directions, including one that makes `readable` true for an empty pipe (the
  lost-wake shape) and one that makes a non-blocking full write return `EAGAIN` where it should
  write what fits.

---

## Task 2: `ALooper`

Seven symbols: `ALooper_prepare`, `_forThread`, `_acquire`, `_release`, `_addFd`, `_removeFd`,
`_pollOnce`. New module `omni-android/src/ndk/looper.rs` — the first thing in this crate that is NDK
rather than bionic or JNI, so `ndk/` is created for it.

### The shape

A looper is **per guest thread**, refcounted, and identified to the guest by a pointer. The registry
is per `Jni`-instance-like state in the `ndk` module, keyed by the same `GuestThreadId` the thread
layer already mints (D24), because two guest instances in one process must not share a looper any
more than they share a `pthread_key` table.

* `ALooper_prepare(opts)` — creates the calling thread's looper if it has none, returns it,
  **acquires** a reference the way the NDK does.
* `ALooper_forThread()` — returns the calling thread's looper or **null**. Null is a real answer
  here, and it is §8.1's fourth failure mode: the host must have prepared one on the thread it calls
  step 13 from. The gate asserts a looper exists *before* the call rather than discovering it from a
  `0` return.
* `_acquire`/`_release` — a refcount, with a release of the last reference destroying the looper and
  a release below zero refusing by name.
* `_addFd(looper, fd, ident, events, callback, data)` — stores a **guest function pointer**.
* `_pollOnce(timeoutMillis, outFd, outEvents, outData)` — consults `Filesystem::readiness` for each
  registered fd; for a registered callback it **calls guest code**, so `ALooper_pollOnce` is
  `bind_reentrant` and everything that reaches it is too (F9).

### The instrumentation §8.1 asks for, built before it is needed

§8.1's fifth failure mode: "the cond-wait means a deadlock here is indistinguishable from a hang;
instrument it." The M3 gate's `OMNI_INIT_WATCHDOG` is the pattern and `Boundary::last_call` is the
one thing that identifies a guest parked inside a handler.

What this task adds, **before** step 13 is first called:

* A **looper event log** per instance — every `addFd`, `removeFd`, `pollOnce` entry and exit, each
  with the calling guest thread id and what it decided. Bounded like `MAX_CALL_RECORDS`, with a
  dropped counter, because a `pollOnce` per frame would otherwise make the log a sample.
* A **cond-wait witness**: the count of guest threads currently blocked in
  `pthread_cond_wait`/`_timedwait`, with the address of the condvar and the calling thread. This is
  in the thread layer, not the looper, and it is what turns "the gate hung" into "thread 0 is parked
  on the condvar at `app+0xf0` and thread 1 last called `ALooper_addFd`".
* A **watchdog** in the gate itself, on the pattern M3's already uses: a host thread that after a
  stated wall-clock budget prints both of the above and fails the test. **It must fail, not print and
  continue** — VERIFICATION entry 4.

The witness is a **detector, not a watch**, and the way to keep that honest is a test that deadlocks
two guest threads on purpose and asserts the witness names both. A counter that only rises under
normal load would be a watch (VERIFICATION entry 11) and must be labelled one if that test cannot be
made to work.

---

## Task 3: `AAssetManager`, `AAsset` and `AConfiguration`

Sixteen symbols. `AAssetManager_fromJava` needs a Java `AssetManager` object, which is a **class
declaration** in the JNI registry plus a host-side binding from that `jobject` to an
`omni_apk::Apk` — the crate already reads assets by their `AAssetManager`-relative name
(`omni-apk/src/apk.rs`), which is exactly the namespace `AAssetManager_open` uses.

* `AAssetManager_open` / `AAsset_read` / `_getLength` / `_close` over the APK reader.
* `AAsset_getBuffer` returns a pointer the **guest** can dereference, so it must map the asset into
  guest memory. That is a map from inside a handler, which F9 forbids on the inline path: it is
  `bind_reentrant`, and the arena it maps into is the one the instance already owns.
* `AAsset_openFileDescriptor` returns a descriptor, an offset and a length. The APK's entries are
  DEFLATED (M0), so there is no file to hand back and no offset that means anything — it **refuses by
  name**, and the refusal says why. This is the one in the group that a plausible stub would break
  silently.
* `AConfiguration_new`/`_delete`/`_fromAssetManager`/`_getLanguage`/`_getCountry`/`_getNavHidden`/
  `_getScreenWidthDp`/`_getScreenHeightDp`/`_getScreenSize` — a host-decided configuration, the same
  `Answer::Unanswered`-until-a-call-site-decides shape as `HwcapPolicy` (D26) and
  `getProcessTimestamp` (D28). The language and country the engine reads are the host's decision and
  are recorded as one.

---

## Task 4: `ANativeWindow` as a type and a refusal

Nine symbols. Step 17, not step 13 — but the type must exist for the glue to store one, and the
`Surface` argument must be a declared Java class. Every function **refuses by name** in this
milestone except `_acquire`/`_release`, which are a refcount on a window that does not exist yet and
therefore also refuse. M6 gives it a real surface.

Stated in the module so it cannot be read as half-finished, the way `omni-bionic`'s `lib.rs` states
the eight thread symbols that belong to the adapter.

---

## Task 5: the M5 gate

`cargo test -p omni-android --release --test gameactivity`, built on `jni_startup.rs`'s `Guest`.

It must:

1. Run steps 1-12 exactly as M4's gate does, reaching `JNI_OnLoad` → `0x00010006` and the 19 of 21.
2. **Prepare a looper on the calling thread** and assert `ALooper_forThread` is non-null *before*
   calling step 13 — §8.1's fourth failure mode made into a precondition rather than a diagnosis.
3. Set `ro.build.version.sdk`.
4. Call `initializeNativeCode` with the three paths, the `AssetManager`, a null `savedState` and a
   `Configuration`, under the watchdog.
5. Assert the returned `jlong` is **non-zero**, and read back out of guest memory the fields §5.2
   names: `sdkVersion` at `+0x30`, `callbacks == this + 0x50` at `+0x00`, the looper at `+0x158`,
   `msgread`/`msgwrite` at `+0x150`/`+0x154`, `assetManager` at `+0x40`, `instance` at `+0x38`.
   **Membership, not a count** — every one of those offsets is from §5.2 and each is a separate
   assertion naming its offset.
6. Report, not assert: what the game thread did, the looper log, the cond-wait witness, the JNI
   misses and the import census. What step 13 needs beyond this is what M5 is measuring.

A missing APK **fails** the test. A watchdog expiry **fails** the test.

---

## Report

Against `docs/DECISIONS.md` as **D29**, with every figure carrying its n and its method, and nothing
entering that file until someone other than its author reproduces it. The questions it must answer:

* Does `initializeNativeCode` return non-zero, and what is at each of §5.2's offsets?
* How many guest threads existed at the end, and did the game thread reach `android_main`?
* What did the engine ask `AConfiguration` and `__system_property_get` for?
* Which imports outside the 188 did it reach? (`BEYOND_THE_PREDICTION` was eight after M4.)
* Is `Configuration.fontScale` asked for as `F` or as `I`? §3.1 says "18 int fields" and Section D
  could not resolve the descriptors — the miss record settles it.
