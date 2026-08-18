# Kiosk Stability and Silent Boot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the kiosk launch the right app automatically, every boot, on both bases — never guessing, never parking, never silently degrading to Roblox's login screen — and turn the already-written silent-boot cosmetics into a verified per-base checklist.

**Architecture:** The launch race dies in the launcher: `resolveGamePackage()` stops guessing and the kiosk WAITS on a `ContentObserver` for `omni_game_package`, retrying the launch on a backoff instead of parking on a bare in-memory boolean. Everything the kiosk could previously fail at silently (device-owner calls, deep-link joins) is recorded in a new `KioskState` and answered back to the host over the existing ordered-broadcast channel, so a launch can fail loudly. Host-side, the fixed `time.sleep()` calls become readiness polls, the pre-session `force-stop` is verified rather than assumed, consent gains a root fallback with a read-back, and a new pure `omnidroid/silentboot.py` turns "the boot shows nothing but our loading screen" into an assertion per base.

**Tech Stack:** Java 11 source / d8 min-api 26 (Gradle-free launcher build via `launcher/build.ps1` + `launcher/build.sh`), Python 3.13 + `unittest` under pytest, adb, QEMU (x86 Bliss OS 16.9.7 base `v6`; ARM LineageOS 23.2 arm64 rooted with Magisk v30.7).

**Spec:** `docs/superpowers/specs/2026-08-19-omni-qemu-and-density-design.md` (§6 — sub-project C; §8 "what gets measured"; §10 sequencing: C runs in parallel with E and depends on nothing in it)

## Global Constraints

- **Both bases, every task.** x86 = Bliss OS 16.9.7, base tag `x86`, base version `v6` (current); ARM = LineageOS 23.2 arm64, base tag `arm`, rooted pair, Magisk v30.7. A change that is only verified on one base is not done.
- **`""` (empty string) is a VALID root mode.** On the x86 Bliss base adbd itself runs as uid 0, so `engine.resolve_root_shell()` returns `""`. Every gate on a root mode MUST be `if su is None:`, NEVER `if not su:`. This exact bug silently disabled a step for months — see `omnidroid/gaming.py:56` and `omnidroid/gaming.py:232-239`.
- **On ARM there is no `adb root`.** LineageOS is a `user` build; root comes only from the Magisk-patched boot. `engine.resolve_su()` probes `/debug_ramdisk/su`, `/sbin/su`, `su`.
- **`ro.*` guest properties cannot be changed on a running instance**, root or not. Anything that needs one is an image rebuild, not a `setprop`.
- **`/data` is ephemeral.** Every production boot is a first boot: app-ops, `pm grant`s and `Settings.Global` writes made last boot are gone. Nothing may assume state survives a power-off unless it is baked into the image.
- **The launcher is built Gradle-free.** `launcher/build.ps1` (Windows) / `launcher/build.sh` (macOS/Linux): aapt2 compile + link → `javac --release 11` → `d8 --min-api 26` → `zipalign` → `apksigner` with `launcher/omni-debug.jks` (storepass and keypass both `omnidroid`). Output: `launcher/build/omni-kiosk.apk`. There is no JUnit and no Gradle — **Java is verified by host-side source-contract tests plus a real on-device run**, never by a Java unit test.
- **The ARM base runs on the Mac at `192.168.0.30` over ssh, and EVERY engine command sent over that ssh session needs `--no-window`.** `engine._host_has_gui()` returns True unconditionally on macOS, but an ssh session is not in the Aqua session, so `-display cocoa` has nothing to attach to and QEMU exits.
- **Test runner** (there is no pytest config in `pyproject.toml`; collection DIES if the config has no base registered — `tests/test_qemu_accepts_devices.py` calls `load_config()` at import time). Copy the config FRESH each run, because the engine writes back to it:
  ```bash
  cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
  OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
    OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
    python -m pytest tests/ -q
  ```
  Baseline: **11 failed / 932 passed**. Diff the `FAILED` lines against the baseline; never count them.
- **Git hygiene:** ~60 uncommitted tracked files of pre-existing WIP live in this repo. NEVER run `git add -A`, `git checkout`, `git stash`, or `git reset`. Every commit step below names its files explicitly.
- **Extend, do not duplicate**, these existing tests: `tests/test_kiosk_boot_app.py` (137 lines), `tests/test_consent.py` (196), `tests/test_session.py` (712), `tests/test_no_console_windows.py` (87).

---

## File Structure

**Launcher (Java, `launcher/src/com/omni/kiosk/`)**

| File | Responsibility |
|---|---|
| `KioskState.java` *(new)* | One in-process record of what the kiosk actually managed to do this boot: device-owner call outcomes, resolved game package, launch attempts, last error. Serialises to JSON. Nothing else in the app holds status. |
| `MainActivity.java` *(modify)* | The launch decision only: resolve (never guess), wait on a `ContentObserver`, retry on a backoff, pin Lock Task from the first frame, keep the screen black until the game is up. Records every outcome into `KioskState`. |
| `SessionReceiver.java` *(modify)* | Adds `ACTION_STATUS`, which answers `KioskState.toJson()` in the ordered-broadcast reply. Session handling itself is unchanged. |
| `OmniSession.java` | **Unchanged.** `join()` already returns `no_deeplink_handler`; the defect is that `MainActivity` swallowed it. |
| `SessionProvider.java`, `OmniDeviceAdminReceiver.java` | **Unchanged.** |
| `launcher/AndroidManifest.xml` *(modify)* | One new `<action>` in the `SessionReceiver` intent-filter. |

**Host (Python, `omnidroid/`)**

| File | Responsibility |
|---|---|
| `silentboot.py` *(new)* | Pure builders/parsers for the per-base boot-cosmetics checklist: the host-side cmdline invariants and the guest probe + its parser. Same shape as `awake.py` / `consent.py` — builds command sequences, never touches adb. |
| `consent.py` *(modify)* | Gains a root-fallback repair script for the app-ops half. Still pure. |
| `engine.py` *(modify)* | Wires all of it: `kiosk_status()` / `assert_kiosk_ready()`, `poll_until()` replacing three fixed sleeps, verified `force-stop`, consent repair, silent-boot report. |

**Tests**

| File | Covers |
|---|---|
| `tests/test_kiosk_boot_app.py` *(extend)* | Tasks 1, 2, 3, 4, 5 — the launcher source contract and the host's status gate. |
| `tests/test_session.py` *(extend)* | Task 6 — the verified force-stop. |
| `tests/test_consent.py` *(extend)* | Task 7 — the root fallback and the `is None` gate. |
| `tests/test_start_timings.py` *(extend)* | Task 8 — `poll_until` and the sleep removals. |
| `tests/test_silent_boot.py` *(new)* | Task 9 — per-base cosmetics invariants. |
| `docs/superpowers/runbooks/2026-08-19-C-silent-boot-checklist.md` *(new)* | Task 9 — the filled-in, dated per-base checklist with real command output. |

---

### Task 1: The kiosk never guesses which app is the game

The recorded failure: `MainActivity.onResume` (line 116) called `resolveGamePackage()` (line 299), which found `omni_game_package` unset, fell back to "first launchable non-system app", picked `com.topjohnwu.magisk`, launched it and PINNED it under Lock Task. Roblox's launch 2.4 s later was refused as a Lock Task Mode violation. `/data` is ephemeral, so every boot is that boot.

The fallback is not deleted — it is the only way a bare `--apk` dev instance launches anything — but it is put behind an explicit opt-in, `Settings.Global omni_kiosk_dev_pick=1`, which production never sets.

**Files:**
- Modify: `launcher/src/com/omni/kiosk/MainActivity.java:43-58` (constants/fields), `:116-132` (`onResume`), `:299-319` (`resolveGamePackage`)
- Test: `tests/test_kiosk_boot_app.py` (append a new class; the file currently ends at line 137)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `MainActivity.DEV_PICK_SETTING = "omni_kiosk_dev_pick"` (a `Settings.Global` int, `1` enables the dev guess); `private boolean devMode()`; `private String resolveGamePackage()` returns the configured package or `null` — never a guess unless `devMode()`. Task 2 calls `resolveGamePackage()` and `devMode()`; Task 5 asserts `omni_kiosk_dev_pick` is absent in production.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_kiosk_boot_app.py` (after the `ClearsTheWrongLockTaskPin` class, before `if __name__ == "__main__":`):

```python
LAUNCHER_SRC = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "launcher", "src", "com", "omni", "kiosk")


def _java(name):
    """One launcher source file as text.

    There is no JUnit and no Gradle in this repo (launcher/build.ps1 is
    aapt2 -> javac -> d8 -> apksigner), so the launcher's contract is pinned
    by reading its source. Weaker than executing it, and deliberately paired
    with the on-device verification written into every task that touches
    Java. Same technique tests/test_private_dns.py uses on Python source."""
    with open(os.path.join(LAUNCHER_SRC, name), encoding="utf-8") as f:
        return f.read()


class TheKioskNeverGuessesInProduction(unittest.TestCase):
    """The 2026-08-06 trace: the fallback picked com.topjohnwu.magisk, pinned
    it under Lock Task, and Roblox was refused 2.4 s later. Production must
    have NO path to that scan."""

    def setUp(self):
        self.src = _java("MainActivity.java")

    def test_the_dev_guess_is_behind_an_explicit_opt_in_setting(self):
        self.assertIn('DEV_PICK_SETTING = "omni_kiosk_dev_pick"', self.src)

    def test_the_installed_app_scan_is_gated_on_dev_mode(self):
        # getInstalledApplications() is the scan that picked Magisk. It must
        # be unreachable unless devMode() said so.
        scan = self.src.index("getInstalledApplications(")
        gate = self.src.index("if (!devMode()) {")
        self.assertLess(gate, scan,
                        "the dev-mode gate must come BEFORE the app scan")

    def test_an_unset_setting_resolves_to_nothing_rather_than_a_guess(self):
        self.assertIn("return null;   // no configuration yet", self.src)

    def test_magisk_can_never_win_even_the_dev_guess(self):
        self.assertIn('"com.topjohnwu.magisk"', self.src)

    def test_onresume_no_longer_launches_inline(self):
        # onResume() called launchGame() directly, which is what made the
        # race a single unrecoverable shot. Task 2 replaces it with a
        # scheduler; this pins that onResume stops deciding.
        self.assertNotIn("launchGame(game, \"boot\")", self.src)
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q -k TheKioskNeverGuesses
```
Expected: FAIL — `AssertionError: 'DEV_PICK_SETTING = "omni_kiosk_dev_pick"' not found`.

- [ ] **Step 3: Add the dev-mode gate to MainActivity**

In `launcher/src/com/omni/kiosk/MainActivity.java`, add beside `GAME_SETTING` (line 45):

```java
    private static final String GAME_SETTING = "omni_game_package";

    /**
     * Opt-in for the dev-mode "first launchable non-system app" guess.
     *
     * THE BUG THIS EXISTS FOR (traced live 2026-08-06 on the rooted arm base):
     *
     *     02:15:40.882  OmniKiosk: launching com.topjohnwu.magisk (boot)
     *     02:15:43.274  settings put global omni_game_package com.roblox.client
     *     02:15:44.904  E ActivityTaskManager: Attempted Lock Task Mode violation
     *                      r=...com.roblox.client/.ActivityProtocolLaunch
     *
     * The guess ran BEFORE the host wrote the setting, picked the Magisk
     * manager (rooting production made it a launchable non-system app), and
     * PINNED it under Lock Task — so the real game was not on the whitelist
     * when its deep link arrived and was refused. Instances are ephemeral, so
     * /data is discarded at power-off and EVERY boot is that boot.
     *
     * A kiosk with nothing to launch shows the loading screen and waits. That
     * is the correct state, not an error, and it is what a guess costs.
     * Production never sets this; `--apk` dev instances do.
     */
    private static final String DEV_PICK_SETTING = "omni_kiosk_dev_pick";
```

Add `com.topjohnwu.magisk` to the `NON_GAME` set (line 50-55), so even the dev guess cannot repeat the recorded failure:

```java
    private static final Set<String> NON_GAME = new HashSet<>(Arrays.asList(
            "net.sourceforge.opencamera",
            "com.termux",
            "com.amaze.filemanager",
            "me.weishu.kernelsu",
            "com.topjohnwu.magisk",
            "xtr.keymapper"));
```

Add `devMode()` immediately above `resolveGamePackage()`:

```java
    /** True only on an instance explicitly put in dev mode. */
    private boolean devMode() {
        try {
            return Settings.Global.getInt(
                    getContentResolver(), DEV_PICK_SETTING, 0) == 1;
        } catch (Throwable t) {
            return false;
        }
    }
```

Replace the body of `resolveGamePackage()` (lines 299-319) with:

```java
    /** The configured game, or null. In DEV MODE ONLY, the first launchable
     *  user app. Never a guess otherwise — see DEV_PICK_SETTING. */
    private String resolveGamePackage() {
        String configured = Settings.Global.getString(
                getContentResolver(), GAME_SETTING);
        PackageManager pm = getPackageManager();
        if (configured != null && !configured.isEmpty()) {
            if (pm.getLaunchIntentForPackage(configured) != null) {
                return configured;
            }
            // Configured but not installed YET. On an offset boot the game is
            // an updated system app in /data and the package manager may still
            // be settling, so this is "not ready", not "wrong" — the caller
            // retries rather than falling back to something else.
            return null;
        }
        if (!devMode()) {
            return null;   // no configuration yet — WAIT, never guess
        }
        List<ApplicationInfo> apps = pm.getInstalledApplications(0);
        for (ApplicationInfo ai : apps) {
            if ((ai.flags & ApplicationInfo.FLAG_SYSTEM) != 0) continue;
            if (ai.packageName.equals(getPackageName())) continue;
            if (NON_GAME.contains(ai.packageName)) continue;
            if (pm.getLaunchIntentForPackage(ai.packageName) != null) {
                return ai.packageName;
            }
        }
        return null;
    }
```

Replace `onResume()` (lines 116-132) with a version that stops deciding inline (Task 2 adds `scheduleLaunch`; for this task it calls `launchGame` through a one-line trampoline so the file still compiles):

```java
    @Override protected void onResume() {
        super.onResume();
        hideSystemUi();
        if (launchedThisBoot) return;
        String game = resolveGamePackage();
        if (game != null) launchGame(game, "boot");
    }
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q
```
Expected: PASS, all classes in the file.

- [ ] **Step 5: Build the APK to prove the Java compiles**

```powershell
powershell -ExecutionPolicy Bypass -File launcher/build.ps1
```
Expected: `BUILT: ...\launcher\build\omni-kiosk.apk (NNNNN bytes)`. A `javac` error here is a task failure, not a warning.

- [ ] **Step 6: Commit**

```bash
git add launcher/src/com/omni/kiosk/MainActivity.java tests/test_kiosk_boot_app.py
git commit -m "fix(kiosk): never guess the game package outside dev mode"
```

---

### Task 2: The kiosk waits for the setting and retries the launch

Two defects, one mechanism. `launchedThisBoot` (line 58) is a bare in-memory boolean with no retry: a first deep link that fails transiently leaves `onResume` showing an empty status and waiting for a human tap — on a headless farming instance, forever. And the host writes `omni_game_package` *after* the kiosk has already resolved, so the kiosk must notice the write.

A `ContentObserver` on `Settings.Global.getUriFor("omni_game_package")` closes the race from the guest side even when the host is late; a `Handler` backoff makes a transient failure recoverable.

**Files:**
- Modify: `launcher/src/com/omni/kiosk/MainActivity.java:57-58` (fields), `:60-75` (`pkgReceiver`), `:77-114` (`onCreate`/`onDestroy`), `:116-132` (`onResume`)
- Test: `tests/test_kiosk_boot_app.py`

**Interfaces:**
- Consumes: `resolveGamePackage()`, `devMode()`, `DEV_PICK_SETTING` (Task 1).
- Produces: `private void scheduleLaunch(long delayMs, String why)`; `private void attemptLaunch(String why)`; `private static long delayFor(int n)`; `private void showStatus(String text)`; fields `launched`, `attempt`, `waits`, `retryScheduled`, `gameSettingObserver`; constants `RETRY_MS` and `MAX_LAUNCH_ATTEMPTS = 8`. Task 3 makes `launchGame` return an error string that `attemptLaunch` consumes. Task 4 reads `attempt` into `KioskState.launchAttempts`.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_kiosk_boot_app.py`:

```python
class ALaunchThatFailsIsRetriedNotParked(unittest.TestCase):
    """launchedThisBoot was a bare boolean with no retry: one transient
    failure and onResume showed an empty status and waited for a tap that
    nobody is there to give."""

    def setUp(self):
        self.src = _java("MainActivity.java")

    def test_the_bare_boolean_is_gone(self):
        self.assertNotIn("launchedThisBoot", self.src)

    def test_there_is_a_backoff_ladder(self):
        self.assertIn("RETRY_MS", self.src)
        self.assertIn("MAX_LAUNCH_ATTEMPTS = 8", self.src)

    def test_retries_are_posted_not_spun(self):
        # postDelayed on the main looper: a busy loop on a 1-vCPU farming
        # guest would take CPU from the thing it is waiting for.
        self.assertIn("postDelayed(", self.src)

    def test_a_late_setting_write_still_launches(self):
        # The host writes omni_game_package ~2.4 s after the kiosk resolved.
        # Observing the setting is what makes that survivable in-guest.
        self.assertIn("registerContentObserver(", self.src)
        self.assertIn("Settings.Global.getUriFor(GAME_SETTING)", self.src)

    def test_the_observer_is_unregistered(self):
        self.assertIn("unregisterContentObserver(", self.src)

    def test_waiting_for_the_setting_is_unbounded(self):
        # A kiosk with nothing to launch shows the loading screen forever;
        # only a FAILING launch is bounded (spec sub-project C, item 1).
        self.assertIn("waits++", self.src)


class TheScreenIsBlackUntilTheGameIsUp(unittest.TestCase):
    """'no apk found' on a customer's panel is a menu with no button."""

    def setUp(self):
        self.src = _java("MainActivity.java")

    def test_the_no_apk_text_is_gone(self):
        self.assertNotIn('"no apk found"', self.src)

    def test_status_text_only_renders_in_dev_mode(self):
        self.assertIn("status.setText(devMode() ? text : \"\");", self.src)
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q \
  -k "ALaunchThatFailsIsRetried or TheScreenIsBlack"
```
Expected: FAIL — `AssertionError: 'launchedThisBoot' unexpectedly found in ...`.

- [ ] **Step 3: Replace the boolean with a scheduler**

Replace the fields at `MainActivity.java:57-58`:

```java
    private TextView status;

    /**
     * Backoff between launch attempts, in ms. The LAST value repeats: a kiosk
     * with nothing to launch polls every 15 s forever, because "waiting for
     * the game package" is the correct state on a boot whose /data is
     * ephemeral and whose host writes the setting seconds later.
     */
    private static final long[] RETRY_MS = {750, 1500, 3000, 6000, 10000, 15000};

    /**
     * A launch that RESOLVED a package and still failed is bounded. Retrying a
     * broken deep link forever would hide the failure behind a black screen;
     * after this many tries the kiosk parks and reports through KioskState so
     * the host can fail the launch loudly instead.
     */
    private static final int MAX_LAUNCH_ATTEMPTS = 8;

    private final android.os.Handler ui =
            new android.os.Handler(android.os.Looper.getMainLooper());
    private boolean launched = false;
    private int attempt = 0;         // launches that actually ran
    private int waits = 0;           // polls while the setting is unset
    private boolean retryScheduled = false;
    private android.database.ContentObserver gameSettingObserver;
```

Replace the tap listener body (lines 90-93):

```java
        status.setOnClickListener(v -> {
            launched = false;
            attempt = 0;
            scheduleLaunch(0L, "tap");
        });
```

Replace the `pkgReceiver` body (lines 60-75):

```java
    private final BroadcastReceiver pkgReceiver = new BroadcastReceiver() {
        @Override public void onReceive(Context c, Intent i) {
            String pkg = i.getData() != null
                    ? i.getData().getSchemeSpecificPart() : null;
            Log.i(TAG, "package event: " + i.getAction() + " " + pkg);
            if (Intent.ACTION_PACKAGE_ADDED.equals(i.getAction())
                    && pkg != null && !pkg.equals(getPackageName())) {
                // A new APK just landed (adb install in dev mode, or an offset
                // settling). Re-arm and try immediately.
                launched = false;
                attempt = 0;
                scheduleLaunch(0L, "new apk installed");
            }
        }
    };
```

At the end of `onCreate` (after `registerReceiver(pkgReceiver, f);`), register the settings observer:

```java
        // THE RACE, closed from the guest side. The host writes
        // omni_game_package AFTER this activity has already resolved (traced:
        // 2.4 s after). Observing the setting means a late write launches the
        // game immediately instead of needing the kiosk to be restarted.
        gameSettingObserver = new android.database.ContentObserver(ui) {
            @Override public void onChange(boolean selfChange) {
                Log.i(TAG, GAME_SETTING + " changed; re-resolving");
                launched = false;
                attempt = 0;
                waits = 0;
                scheduleLaunch(0L, "game package changed");
            }
        };
        getContentResolver().registerContentObserver(
                Settings.Global.getUriFor(GAME_SETTING), false,
                gameSettingObserver);
```

Replace `onDestroy` (lines 106-109):

```java
    @Override protected void onDestroy() {
        unregisterReceiver(pkgReceiver);
        if (gameSettingObserver != null) {
            getContentResolver().unregisterContentObserver(gameSettingObserver);
        }
        ui.removeCallbacksAndMessages(null);
        super.onDestroy();
    }
```

Replace `onResume` with:

```java
    @Override protected void onResume() {
        super.onResume();
        hideSystemUi();
        if (!launched) scheduleLaunch(0L, "boot");
    }
```

Add the scheduler, immediately above `launchGame`:

```java
    /** Queue one launch attempt on the main looper. Coalesces: a retry
     *  already in flight is not doubled by a resume or a package event. */
    private void scheduleLaunch(long delayMs, final String why) {
        if (launched || retryScheduled) return;
        retryScheduled = true;
        ui.postDelayed(new Runnable() {
            @Override public void run() {
                retryScheduled = false;
                attemptLaunch(why);
            }
        }, delayMs);
    }

    /** Resolve, launch, and decide what happens if that did not work. */
    private void attemptLaunch(String why) {
        if (launched) return;
        String game = resolveGamePackage();
        if (game == null) {
            // Nothing to launch YET. Unbounded on purpose: the loading screen
            // is the correct state for a kiosk with no game, and the observer
            // above wakes us the moment the setting lands.
            waits++;
            showStatus("");
            scheduleLaunch(delayFor(waits), "waiting for " + GAME_SETTING);
            return;
        }
        attempt++;
        String err = launchGame(game, why);
        if (err == null) {
            launched = true;
            attempt = 0;
            waits = 0;
            return;
        }
        Log.w(TAG, "launch attempt " + attempt + " for " + game
                + " failed: " + err);
        if (attempt >= MAX_LAUNCH_ATTEMPTS) {
            showStatus(err);
            return;
        }
        scheduleLaunch(delayFor(attempt), why);
    }

    /** nth delay from the ladder; the last value repeats. */
    private static long delayFor(int n) {
        int i = n - 1;
        if (i < 0) i = 0;
        if (i >= RETRY_MS.length) i = RETRY_MS.length - 1;
        return RETRY_MS[i];
    }

    /** Production shows NOTHING but black until the game is on screen: a
     *  status string on a customer's panel is a menu they cannot dismiss.
     *  Dev instances (omni_kiosk_dev_pick=1) still get the text. */
    private void showStatus(String text) {
        status.setText(devMode() ? text : "");
    }
```

Finally, make `launchGame` return `String` (null on success) so `attemptLaunch` compiles. Replace lines 328-355 — Task 3 rewrites the Roblox branch of this method, so write it now as:

```java
    private String launchGame(String pkg, String why) {
        whitelistForLockTask(pkg);
        enterLockTask();
        if (OmniSession.ROBLOX_PACKAGE.equals(pkg)
                && OmniSession.hasSession(this)) {
            String err = OmniSession.join(this, why);
            if (err == null) {
                showStatus("");
                return null;
            }
            Log.w(TAG, "deep-link join failed (" + err + ")");
            return "join_" + err;
        }
        Intent li = getPackageManager().getLaunchIntentForPackage(pkg);
        if (li == null) return "no_launch_intent";
        Log.i(TAG, "launching " + pkg + " (" + why + ")");
        showStatus("");
        li.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            startActivity(li);
        } catch (Exception e) {
            Log.w(TAG, "startActivity failed: " + e);
            return "start_failed";
        }
        return null;
    }
```

- [ ] **Step 4: Run the tests and the build**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q
powershell -ExecutionPolicy Bypass -File launcher/build.ps1
```
Expected: PASS, then `BUILT: ...omni-kiosk.apk`.

- [ ] **Step 5: Verify on a live x86 instance**

```bash
omnidroid start u1
adb -s 127.0.0.1:16001 install -r launcher/build/omni-kiosk.apk
adb -s 127.0.0.1:16001 shell settings delete global omni_game_package
adb -s 127.0.0.1:16001 shell am force-stop com.omni.kiosk
adb -s 127.0.0.1:16001 shell am start -n com.omni.kiosk/.MainActivity
adb -s 127.0.0.1:16001 logcat -d -s OmniKiosk | tail -20
```
Expected: repeated `waiting for omni_game_package` scheduling and NO `launching com.topjohnwu.magisk`. Then:
```bash
adb -s 127.0.0.1:16001 shell settings put global omni_game_package com.roblox.client
adb -s 127.0.0.1:16001 logcat -d -s OmniKiosk | tail -5
```
Expected: `omni_game_package changed; re-resolving` followed by a launch line, WITHOUT restarting the kiosk.

- [ ] **Step 6: Commit**

```bash
git add launcher/src/com/omni/kiosk/MainActivity.java tests/test_kiosk_boot_app.py
git commit -m "fix(kiosk): observe the game setting and retry launches on a backoff"
```

---

### Task 3: A failed deep-link join is reported, never degraded to the login screen

`OmniSession.join` returns `no_deeplink_handler` when `resolveActivity` is null (`OmniSession.java:154`). `MainActivity.launchGame` used to log that and fall through to `getLaunchIntentForPackage(pkg)` — which opens **Roblox's own login screen**, not the joined place. The instance then looks launched and is not playing. Spec §8: "an instance is really farming — USER time, not 'the process exists'".

The fallback is kept for one case only, and it is the honest one: **no session at all**. With a session present, a join failure is an error that propagates to the host.

**Files:**
- Modify: `launcher/src/com/omni/kiosk/MainActivity.java` (`launchGame`, written in Task 2 Step 3)
- Test: `tests/test_kiosk_boot_app.py`

**Interfaces:**
- Consumes: `launchGame(String pkg, String why) -> String` (Task 2); `OmniSession.join(Context, String) -> String` and `OmniSession.hasSession(Context) -> boolean` (existing, unchanged).
- Produces: `launchGame` returns `"join_" + err` where `err` is one of `roblox_not_installed`, `no_place`, `no_deeplink_handler`, `start_failed`. Task 4 stores that string in `KioskState.lastError`; Task 5 surfaces it to the host.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_kiosk_boot_app.py`:

```python
class AFailedJoinIsNeverDegradedToTheLoginScreen(unittest.TestCase):
    """OmniSession.join returns no_deeplink_handler when the roblox:// handler
    is missing. Falling through to getLaunchIntentForPackage() opens Roblox's
    OWN LOGIN SCREEN and reports a successful launch — 'running' instead of
    'playing'. With a session in hand that is a failure, not a fallback."""

    def setUp(self):
        self.src = _java("MainActivity.java")

    def test_the_silent_fallback_comment_is_gone(self):
        self.assertNotIn("falling back to the launcher intent", self.src)

    def test_a_join_failure_returns_a_prefixed_reason(self):
        self.assertIn('return "join_" + err;', self.src)

    def test_the_plain_launcher_intent_is_unreachable_after_a_join_failure(self):
        # The `return` must sit between the join failure and the launcher
        # intent, so no control flow reaches the login screen with a session.
        ret = self.src.index('return "join_" + err;')
        intent = self.src.index("getLaunchIntentForPackage(pkg)")
        self.assertLess(ret, intent)

    def test_no_session_still_uses_the_plain_launcher_intent(self):
        # A plain APK under test has no session and no deep link; it must
        # still launch normally.
        self.assertIn("OmniSession.hasSession(this)", self.src)


class TheJoinContractIsUnchanged(unittest.TestCase):
    """OmniSession already reports honestly; the defect was the caller."""

    def test_join_still_reports_a_missing_deeplink_handler(self):
        self.assertIn('return "no_deeplink_handler";', _java("OmniSession.java"))
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q -k AFailedJoin
```
Expected: FAIL on `test_the_silent_fallback_comment_is_gone` if Task 2 was skipped; if Task 2 landed, the remaining assertions still need the doc comment below to make the intent explicit — run it and confirm the run is red before editing.

- [ ] **Step 3: Document the contract at the call site**

Replace the Roblox branch inside `launchGame` with the commented final form:

```java
        if (OmniSession.ROBLOX_PACKAGE.equals(pkg)
                && OmniSession.hasSession(this)) {
            String err = OmniSession.join(this, why);
            if (err == null) {
                showStatus("");
                return null;
            }
            // NO SILENT DEGRADE. The old code fell through to the plain
            // launcher intent here, which opens Roblox's OWN LOGIN SCREEN and
            // then reported a successful launch. An instance sitting on a
            // login screen is not playing, and the whole product promise is a
            // boot that lands IN the place with no menu and no taps. So a
            // session that cannot be joined is an ERROR the host is told
            // about (KioskState -> the STATUS broadcast), not a screen the
            // customer is shown. A package with NO session still takes the
            // launcher-intent path below — that is a plain APK under test.
            Log.w(TAG, "deep-link join failed (" + err + ")");
            return "join_" + err;
        }
```

- [ ] **Step 4: Run the tests and the build**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q
powershell -ExecutionPolicy Bypass -File launcher/build.ps1
```
Expected: PASS; `BUILT: ...omni-kiosk.apk`.

- [ ] **Step 5: Commit**

```bash
git add launcher/src/com/omni/kiosk/MainActivity.java tests/test_kiosk_boot_app.py
git commit -m "fix(kiosk): a failed deep-link join is an error, not the login screen"
```

---

### Task 4: KioskState — device-owner failures stop being invisible

Nearly every device-owner call is wrapped in a try/catch that only logs: `MainActivity.java:251` (`setPermissionPolicy`), `:264` (`setLockTaskFeatures`), `:267` (`setStatusBarDisabled`), `:283` (`startLockTask`), `:370` (`setLockTaskPackages` in `whitelistForLockTask`). Two of them (`:264`, `:267`) are `catch (Throwable ignore) { }` — literally nothing. If `setStatusBarDisabled` fails, the status bar swipes down over the game and nobody finds out.

This task adds the record and the channel. Task 5 makes the host act on it.

It also enters Lock Task from `onCreate` rather than only from `launchGame`, so the status bar and nav gestures are blocked from the first frame instead of from the first successful launch.

**Files:**
- Create: `launcher/src/com/omni/kiosk/KioskState.java`
- Modify: `launcher/src/com/omni/kiosk/MainActivity.java:77-104` (`onCreate`), `:216-226` (`ensureBlackWallpaper`), `:232-286` (`configureLockTask`, `enterLockTask`), `:358-371` (`whitelistForLockTask`)
- Modify: `launcher/src/com/omni/kiosk/SessionReceiver.java:199-225`
- Modify: `launcher/AndroidManifest.xml:61-64`
- Test: `tests/test_kiosk_boot_app.py`

**Interfaces:**
- Consumes: `attempt` / `launched` / `showStatus` (Task 2); `launchGame` error strings (Task 3).
- Produces:
  - `KioskState` static fields: `activityCreated`, `deviceOwner`, `lockTaskPackagesSet`, `lockTaskFeaturesSet`, `statusBarDisabled`, `permissionPolicySet`, `lockTaskEntered`, `blackWallpaper`, `keyguardDisabled` (all `boolean`); `gamePackage`, `status`, `lastError` (`String`); `launchAttempts` (`int`).
  - `KioskState.STATUS_WAITING = "waiting_for_game_package"`, `STATUS_LAUNCHING = "launching"`, `STATUS_LAUNCHED = "launched"`, `STATUS_FAILED = "launch_failed"`.
  - `static void policyFailed(String call, Throwable t)`, `static String policyFailures()`, `static JSONObject toJson()`.
  - `SessionReceiver.ACTION_STATUS = "com.omni.kiosk.STATUS"`, answering `{"ok":true,"kiosk":{...}}`. Task 5 parses exactly these key names.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_kiosk_boot_app.py`:

```python
class TheKioskReportsWhatItActuallyManagedToDo(unittest.TestCase):
    """Five device-owner calls were wrapped in catch-and-log (two of them in
    `catch (Throwable ignore) { }`). A setStatusBarDisabled that fails means
    the customer can swipe Quick Settings over the game — and nothing said so."""

    def test_there_is_a_state_class(self):
        self.assertIn("class KioskState", _java("KioskState.java"))

    def test_every_policy_call_records_its_outcome(self):
        src = _java("MainActivity.java")
        for call in ("setPermissionPolicy", "setLockTaskPackages",
                     "setLockTaskFeatures", "setStatusBarDisabled"):
            self.assertIn(f'KioskState.policyFailed("{call}"', src,
                          f"{call} still fails silently")

    def test_nothing_is_swallowed_by_an_empty_catch(self):
        src = _java("MainActivity.java")
        self.assertNotIn("catch (Throwable ignore) { }", src)

    def test_the_state_serialises_the_keys_the_host_reads(self):
        src = _java("KioskState.java")
        for key in ("activity_created", "device_owner",
                    "lock_task_packages_set", "status_bar_disabled",
                    "permission_policy_set", "lock_task_entered",
                    "black_wallpaper", "keyguard_disabled",
                    "game_package", "status", "launch_attempts",
                    "last_error", "policy_failures"):
            self.assertIn(f'"{key}"', src)

    def test_the_status_action_answers_the_state(self):
        src = _java("SessionReceiver.java")
        self.assertIn('ACTION_STATUS = "com.omni.kiosk.STATUS"', src)
        self.assertIn("KioskState.toJson()", src)

    def test_the_status_action_is_in_the_manifest(self):
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        with open(os.path.join(root, "launcher", "AndroidManifest.xml"),
                  encoding="utf-8") as f:
            self.assertIn('android:name="com.omni.kiosk.STATUS"', f.read())

    def test_lock_task_is_entered_from_oncreate_not_only_from_a_launch(self):
        # Otherwise the status bar is pullable during the whole wait for
        # omni_game_package — which is exactly the boot the user complained
        # about ("don't show the top bar that comes when swiping down").
        src = _java("MainActivity.java")
        create = src.index("protected void onCreate(")
        resume = src.index("protected void onResume(")
        self.assertLess(create, src.index("enterLockTask();"))
        self.assertLess(src.index("enterLockTask();"), resume)
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q -k TheKioskReportsWhat
```
Expected: FAIL with `FileNotFoundError: ...launcher/src/com/omni/kiosk/KioskState.java`.

- [ ] **Step 3: Create KioskState.java**

```java
package com.omni.kiosk;

import org.json.JSONException;
import org.json.JSONObject;

/**
 * What the kiosk ACTUALLY managed to do this boot.
 *
 * Every device-owner call in MainActivity used to be wrapped in a try/catch
 * that only logged — two of them in `catch (Throwable ignore) { }`. So a
 * setStatusBarDisabled() that failed produced a running instance whose status
 * bar could be swiped down over the game, and the host had no way to know.
 * The product promise is a screen with no menus on it; a policy that fails
 * quietly is that promise failing quietly.
 *
 * This is process-local, in-memory state — never persisted. /data is
 * ephemeral, so persisting it would be a lie on the next boot anyway, and the
 * question it answers ("is THIS running kiosk healthy?") is only meaningful
 * for the life of the process. `activityCreated` exists because a STATUS
 * broadcast can cold-start the kiosk's process without MainActivity ever
 * having run: a reader that sees activityCreated=false is looking at a blank
 * record, not at a broken kiosk, and must say so.
 */
public final class KioskState {

    public static final String STATUS_WAITING = "waiting_for_game_package";
    public static final String STATUS_LAUNCHING = "launching";
    public static final String STATUS_LAUNCHED = "launched";
    public static final String STATUS_FAILED = "launch_failed";

    static volatile boolean activityCreated = false;
    static volatile boolean deviceOwner = false;
    static volatile boolean lockTaskPackagesSet = false;
    static volatile boolean lockTaskFeaturesSet = false;
    static volatile boolean statusBarDisabled = false;
    static volatile boolean permissionPolicySet = false;
    static volatile boolean lockTaskEntered = false;
    static volatile boolean blackWallpaper = false;
    static volatile boolean keyguardDisabled = false;
    static volatile String gamePackage = null;
    static volatile String status = STATUS_WAITING;
    static volatile String lastError = null;
    static volatile int launchAttempts = 0;

    private static final StringBuilder FAILURES = new StringBuilder();

    private KioskState() { }

    /** Record (and log) a policy call that did not work. */
    static synchronized void policyFailed(String call, Throwable t) {
        if (FAILURES.length() > 0) FAILURES.append("; ");
        FAILURES.append(call).append(": ").append(t);
        android.util.Log.w("OmniKiosk",
                "device-owner call FAILED: " + call + ": " + t);
    }

    static synchronized String policyFailures() {
        return FAILURES.length() == 0 ? null : FAILURES.toString();
    }

    /** The exact shape engine.kiosk_status() parses. */
    static synchronized JSONObject toJson() throws JSONException {
        JSONObject o = new JSONObject();
        o.put("activity_created", activityCreated);
        o.put("device_owner", deviceOwner);
        o.put("lock_task_packages_set", lockTaskPackagesSet);
        o.put("lock_task_features_set", lockTaskFeaturesSet);
        o.put("status_bar_disabled", statusBarDisabled);
        o.put("permission_policy_set", permissionPolicySet);
        o.put("lock_task_entered", lockTaskEntered);
        o.put("black_wallpaper", blackWallpaper);
        o.put("keyguard_disabled", keyguardDisabled);
        o.put("game_package", gamePackage == null
                ? JSONObject.NULL : gamePackage);
        o.put("status", status);
        o.put("launch_attempts", launchAttempts);
        o.put("last_error", lastError == null ? JSONObject.NULL : lastError);
        String f = policyFailures();
        o.put("policy_failures", f == null ? JSONObject.NULL : f);
        return o;
    }
}
```

- [ ] **Step 4: Wire MainActivity into KioskState**

In `onCreate`, after `setContentView(status);`, add:

```java
        KioskState.activityCreated = true;
```

Replace `dismissKeyguard()`'s device-owner block (lines 197-210) so the outcome is recorded:

```java
        try {
            DevicePolicyManager dpm = getSystemService(DevicePolicyManager.class);
            if (dpm != null && dpm.isDeviceOwnerApp(getPackageName())) {
                ComponentName admin =
                        new ComponentName(this, OmniDeviceAdminReceiver.class);
                // Returns false (rather than throwing) when a secure lock
                // credential is set — these images have none.
                boolean off = dpm.setKeyguardDisabled(admin, true);
                KioskState.keyguardDisabled = off;
                if (!off) {
                    KioskState.policyFailed("setKeyguardDisabled",
                            new IllegalStateException("returned false"));
                }
                Log.i(TAG, "device owner: keyguard disabled = " + off);
            }
        } catch (Throwable t) {
            KioskState.policyFailed("setKeyguardDisabled", t);
        }
```

Replace `ensureBlackWallpaper` (lines 216-226):

```java
    private void ensureBlackWallpaper() {
        try {
            WallpaperManager wm = WallpaperManager.getInstance(this);
            Bitmap black = Bitmap.createBitmap(1, 1, Bitmap.Config.ARGB_8888);
            black.eraseColor(Color.BLACK);
            wm.setBitmap(black);
            black.recycle();
            KioskState.blackWallpaper = true;
        } catch (Exception e) {
            KioskState.policyFailed("setWallpaper", e);
        }
    }
```

Replace `configureLockTask` entirely (lines 232-273):

```java
    /** If the kiosk is device owner, whitelist itself + the game for Lock
     *  Task Mode and disable the status bar. Lock Task fully blocks the
     *  status bar / Quick-Settings pull-down / nav gestures — immersive
     *  mode alone only hides the bar (it can be swiped back).
     *
     *  Every call here records its outcome. These are not decorative: a
     *  failed setLockTaskPackages is the 2026-08-06 Lock Task violation, and
     *  a failed setStatusBarDisabled is a Quick-Settings panel over the game. */
    private void configureLockTask() {
        DevicePolicyManager dpm = getSystemService(DevicePolicyManager.class);
        if (dpm == null || !dpm.isDeviceOwnerApp(getPackageName())) {
            KioskState.deviceOwner = false;
            KioskState.policyFailed("device_owner", new IllegalStateException(
                    "the kiosk is not device owner on this image"));
            return;
        }
        KioskState.deviceOwner = true;
        ComponentName admin =
                new ComponentName(this, OmniDeviceAdminReceiver.class);
        // Auto-grant runtime permissions device-wide. Without this, Roblox
        // stops on Android 13+ with "Allow Roblox to send you notifications?"
        // — a dialog sitting on top of the game that someone has to tap.
        try {
            dpm.setPermissionPolicy(
                    admin, DevicePolicyManager.PERMISSION_POLICY_AUTO_GRANT);
            KioskState.permissionPolicySet = true;
        } catch (Throwable t) {
            KioskState.policyFailed("setPermissionPolicy", t);
        }
        String game = resolveGamePackage();
        KioskState.gamePackage = game;
        String[] pkgs = (game != null)
                ? new String[]{getPackageName(), game}
                : new String[]{getPackageName()};
        try {
            dpm.setLockTaskPackages(admin, pkgs);
            KioskState.lockTaskPackagesSet = true;
        } catch (Throwable t) {
            KioskState.policyFailed("setLockTaskPackages", t);
        }
        try {
            // Disable every lock-task escape surface: no status bar, no
            // notifications, no home/recents, no system info.
            dpm.setLockTaskFeatures(admin,
                    DevicePolicyManager.LOCK_TASK_FEATURE_NONE);
            KioskState.lockTaskFeaturesSet = true;
        } catch (Throwable t) {
            KioskState.policyFailed("setLockTaskFeatures", t);
        }
        try {
            dpm.setStatusBarDisabled(admin, true);
            KioskState.statusBarDisabled = true;
        } catch (Throwable t) {
            KioskState.policyFailed("setStatusBarDisabled", t);
        }
        Log.i(TAG, "device owner: lock task configured for "
                + java.util.Arrays.toString(pkgs));
    }
```

Replace `enterLockTask` (lines 276-286):

```java
    /** Enter Lock Task (pinning). Safe to call repeatedly. */
    private void enterLockTask() {
        try {
            DevicePolicyManager dpm =
                    getSystemService(DevicePolicyManager.class);
            if (dpm != null && dpm.isLockTaskPermitted(getPackageName())) {
                startLockTask();
                KioskState.lockTaskEntered = true;
            }
        } catch (Exception e) {
            KioskState.policyFailed("startLockTask", e);
        }
    }
```

Replace `whitelistForLockTask` (lines 358-371):

```java
    /** Allow the kiosk + the game to run inside Lock Task Mode. */
    private void whitelistForLockTask(String pkg) {
        try {
            DevicePolicyManager dpm =
                    getSystemService(DevicePolicyManager.class);
            if (dpm != null && dpm.isDeviceOwnerApp(getPackageName())) {
                ComponentName admin = new ComponentName(
                        this, OmniDeviceAdminReceiver.class);
                dpm.setLockTaskPackages(admin,
                        new String[]{getPackageName(), pkg});
                KioskState.lockTaskPackagesSet = true;
            }
        } catch (Exception e) {
            KioskState.policyFailed("setLockTaskPackages", e);
        }
    }
```

In `onCreate`, pin from the first frame — replace `configureLockTask();` (line 97) with:

```java
        configureLockTask();
        // Pin NOW, not at the first successful launch. The kiosk can wait
        // minutes for omni_game_package on a slow boot, and an unpinned kiosk
        // still has a pull-down status bar.
        enterLockTask();
```

Finally, have `attemptLaunch` (Task 2) keep `KioskState` current. Replace its body's bookkeeping lines:

```java
        if (game == null) {
            waits++;
            KioskState.status = KioskState.STATUS_WAITING;
            showStatus("");
            scheduleLaunch(delayFor(waits), "waiting for " + GAME_SETTING);
            return;
        }
        attempt++;
        KioskState.gamePackage = game;
        KioskState.launchAttempts = attempt;
        KioskState.status = KioskState.STATUS_LAUNCHING;
        String err = launchGame(game, why);
        if (err == null) {
            launched = true;
            attempt = 0;
            waits = 0;
            KioskState.status = KioskState.STATUS_LAUNCHED;
            KioskState.lastError = null;
            return;
        }
        KioskState.lastError = err;
        Log.w(TAG, "launch attempt " + attempt + " for " + game
                + " failed: " + err);
        if (attempt >= MAX_LAUNCH_ATTEMPTS) {
            KioskState.status = KioskState.STATUS_FAILED;
            showStatus(err);
            return;
        }
        scheduleLaunch(delayFor(attempt), why);
```

- [ ] **Step 5: Add ACTION_STATUS to SessionReceiver and the manifest**

In `SessionReceiver.java`, beside the existing actions (lines 199-200):

```java
    public static final String ACTION_SET_SESSION = "com.omni.kiosk.SET_SESSION";
    public static final String ACTION_CLEAR_SESSION = "com.omni.kiosk.CLEAR_SESSION";
    /** Read-only health check: what the kiosk actually managed to do this
     *  boot. The host calls this before it declares a launch successful. */
    public static final String ACTION_STATUS = "com.omni.kiosk.STATUS";
```

In `onReceive`, add the branch before the `else`:

```java
            } else if (ACTION_STATUS.equals(action)) {
                out.put("ok", true).put("kiosk", KioskState.toJson());
            } else {
```

In `launcher/AndroidManifest.xml`, inside the `SessionReceiver` intent-filter (lines 61-64):

```xml
            <intent-filter>
                <action android:name="com.omni.kiosk.SET_SESSION" />
                <action android:name="com.omni.kiosk.CLEAR_SESSION" />
                <action android:name="com.omni.kiosk.STATUS" />
            </intent-filter>
```

- [ ] **Step 6: Run the tests and the build**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q
powershell -ExecutionPolicy Bypass -File launcher/build.ps1
```
Expected: PASS; `BUILT: ...omni-kiosk.apk`.

- [ ] **Step 7: Verify the reply on a live instance**

```bash
adb -s 127.0.0.1:16001 install -r launcher/build/omni-kiosk.apk
adb -s 127.0.0.1:16001 shell am start -n com.omni.kiosk/.MainActivity
adb -s 127.0.0.1:16001 shell am broadcast -a com.omni.kiosk.STATUS \
    -n com.omni.kiosk/.SessionReceiver
```
Expected: `Broadcast completed: result=-1, data="{"ok":true,"kiosk":{"activity_created":true,"device_owner":true,...}}"`.

Cross-check the two policies that matter against the platform's own view:
```bash
adb -s 127.0.0.1:16001 shell dumpsys activity activities | grep -m2 "mLockTask"
```
Expected: `mLockTaskModeState=LOCKED` (or `PINNED`) and `mLockTaskPackages ... com.omni.kiosk, com.roblox.client` — never the empty list from the recorded failure.

- [ ] **Step 8: Commit**

```bash
git add launcher/src/com/omni/kiosk/KioskState.java \
        launcher/src/com/omni/kiosk/MainActivity.java \
        launcher/src/com/omni/kiosk/SessionReceiver.java \
        launcher/AndroidManifest.xml tests/test_kiosk_boot_app.py
git commit -m "feat(kiosk): report device-owner and launch state back to the host"
```

---

### Task 5: The host fails a launch loudly when the kiosk is not healthy

The reply from Task 4 is useless unless something reads it. `_ensure_booted` gains a check right after `assert_kiosk_game()`: ask the kiosk what it managed to do, and if the answers that matter are false, say so in the launch result rather than returning a "successful" boot to a black screen.

`omnidroid update-kiosk` must also be run so a shipped base carries the new APK — the fast loop is `adb install -r`, the durable one is the base refresh.

**Files:**
- Modify: `omnidroid/engine.py:8114-8117` (action constants), `:8305` region (new functions above `deliver_session`), `:10300` (call site in `_ensure_booted`)
- Test: `tests/test_kiosk_boot_app.py`

**Interfaces:**
- Consumes: `KioskState.toJson()` key names (Task 4); existing `engine.kiosk_broadcast(acct, action, extras=None, timeout=120) -> (dict|None, str)`; existing `engine.kiosk_installed(acct) -> bool`.
- Produces:
  - `engine.KIOSK_ACTION_STATUS = "com.omni.kiosk.STATUS"`
  - `engine.kiosk_status(acct, timeout=45) -> dict | None`
  - `engine.KIOSK_REQUIRED_POLICY: tuple[str, ...]`
  - `engine.kiosk_policy_problems(state: dict | None) -> list[str]`
  - `engine.assert_kiosk_ready(acct, label) -> dict` with keys `ready` (bool), `state` (dict|None), `problems` (list[str])

- [ ] **Step 1: Write the failing test**

Append to `tests/test_kiosk_boot_app.py`:

```python
_HEALTHY = {
    "activity_created": True, "device_owner": True,
    "lock_task_packages_set": True, "lock_task_features_set": True,
    "status_bar_disabled": True, "permission_policy_set": True,
    "lock_task_entered": True, "black_wallpaper": True,
    "keyguard_disabled": True, "game_package": "com.roblox.client",
    "status": "launched", "launch_attempts": 0,
    "last_error": None, "policy_failures": None,
}


def _state(**over):
    s = dict(_HEALTHY)
    s.update(over)
    return s


class TheHostReadsTheKiosksAnswer(unittest.TestCase):
    def test_a_healthy_kiosk_has_no_problems(self):
        self.assertEqual(omni.kiosk_policy_problems(_state()), [])

    def test_no_reply_at_all_is_a_problem(self):
        self.assertEqual(
            omni.kiosk_policy_problems(None),
            ["the kiosk did not answer a status broadcast"])

    def test_a_cold_receiver_process_is_named_as_such(self):
        # A STATUS broadcast can start the kiosk's process without
        # MainActivity ever running; that record is BLANK, not broken, and
        # reporting it as "not device owner" would send someone hunting a
        # bug that is not there.
        self.assertEqual(
            omni.kiosk_policy_problems(_state(activity_created=False)),
            ["the kiosk activity has not started (it is not HOME)"])

    def test_a_failed_status_bar_lockdown_is_a_problem(self):
        self.assertIn("status_bar_disabled",
                      omni.kiosk_policy_problems(_state(status_bar_disabled=False)))

    def test_a_failed_lock_task_whitelist_is_a_problem(self):
        self.assertIn("lock_task_packages_set",
                      omni.kiosk_policy_problems(
                          _state(lock_task_packages_set=False)))

    def test_a_recorded_policy_failure_is_reported_verbatim(self):
        probs = omni.kiosk_policy_problems(
            _state(policy_failures="setStatusBarDisabled: SecurityException"))
        self.assertTrue(any("SecurityException" in p for p in probs), probs)

    def test_a_parked_launch_is_a_problem(self):
        probs = omni.kiosk_policy_problems(
            _state(status="launch_failed", last_error="join_no_deeplink_handler"))
        self.assertTrue(any("join_no_deeplink_handler" in p for p in probs),
                        probs)

    def test_still_waiting_for_the_game_package_is_not_an_error(self):
        # It is the correct state at the moment the check runs — the host
        # writes the setting moments later and the observer picks it up.
        self.assertEqual(
            omni.kiosk_policy_problems(
                _state(status="waiting_for_game_package", game_package=None)),
            [])


class TheStatusBroadcastIsSentAndParsed(unittest.TestCase):
    def test_it_asks_for_the_status_action(self):
        seen = {}

        def fake(acct, action, extras=None, timeout=120):
            seen["action"] = action
            return {"ok": True, "kiosk": _state()}, ""

        with mock.patch.object(omni, "kiosk_broadcast", side_effect=fake):
            st = omni.kiosk_status(_acct())
        self.assertEqual(seen["action"], "com.omni.kiosk.STATUS")
        self.assertEqual(st["game_package"], "com.roblox.client")

    def test_a_reply_without_a_kiosk_block_is_none(self):
        with mock.patch.object(omni, "kiosk_broadcast",
                               return_value=({"ok": True}, "")):
            self.assertIsNone(omni.kiosk_status(_acct()))

    def test_a_timeout_is_none_not_an_exception(self):
        with mock.patch.object(omni, "kiosk_broadcast",
                               return_value=(None, "<no reply>")):
            self.assertIsNone(omni.kiosk_status(_acct()))


class TheBootTailChecksTheKiosk(unittest.TestCase):
    def test_assert_kiosk_ready_is_called_from_the_shared_boot_tail(self):
        import inspect
        src = inspect.getsource(omni._ensure_booted)
        self.assertIn("assert_kiosk_ready(", src)
        # AFTER assert_kiosk_game: the setting has to be written before the
        # kiosk can be expected to have resolved it.
        self.assertLess(src.index("assert_kiosk_game("),
                        src.index("assert_kiosk_ready("))

    def test_it_never_raises_into_the_boot(self):
        with mock.patch.object(omni, "kiosk_installed", return_value=True), \
             mock.patch.object(omni, "kiosk_status",
                               side_effect=RuntimeError("adb died")):
            r = omni.assert_kiosk_ready(_acct(), "t")
        self.assertFalse(r["ready"])
        self.assertTrue(r["problems"])

    def test_an_absent_kiosk_is_reported_not_probed(self):
        with mock.patch.object(omni, "kiosk_installed", return_value=False):
            r = omni.assert_kiosk_ready(_acct(), "t")
        self.assertFalse(r["ready"])
        self.assertIn("kiosk_missing", r["problems"])
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q -k "TheHostReads or TheStatusBroadcast or TheBootTail"
```
Expected: FAIL with `AttributeError: module 'omnidroid.engine' has no attribute 'kiosk_policy_problems'`.

- [ ] **Step 3: Implement the host side**

In `omnidroid/engine.py`, beside the other kiosk action constants (lines 8114-8117):

```python
KIOSK_ACTION_SET_SESSION = "com.omni.kiosk.SET_SESSION"
KIOSK_ACTION_CLEAR_SESSION = "com.omni.kiosk.CLEAR_SESSION"
KIOSK_ACTION_STATUS = "com.omni.kiosk.STATUS"
```

Add above `def deliver_session(...)` (line 8305):

```python
# The device-owner outcomes whose failure changes what a customer SEES. Not
# every field KioskState reports: `lock_task_features_set` and
# `black_wallpaper` are cosmetic belts whose braces (immersive mode, the baked
# wallpaper, the boot animation) are applied host-side anyway, and failing a
# launch over one of them would trade a real instance for a tidy report.
KIOSK_REQUIRED_POLICY = (
    "device_owner",             # nothing below is possible without it
    "lock_task_packages_set",   # THE 2026-08-06 bug: a wrong/empty whitelist
                                # is what refused Roblox's deep link
    "status_bar_disabled",      # a pull-down Quick Settings over the game
    "permission_policy_set",    # the POST_NOTIFICATIONS dialog on top of it
)


def kiosk_status(acct, timeout=45):
    """What the kiosk says it actually managed to do this boot, or None.

    None means "it did not answer", which is a DIFFERENT thing from "it
    answered badly" — see kiosk_policy_problems. Never raises: a status probe
    that fails must not be the thing that takes a booted instance down.
    """
    try:
        reply, _raw = kiosk_broadcast(acct, KIOSK_ACTION_STATUS, timeout=timeout)
    except Exception:  # noqa: BLE001 — a probe must never break a boot
        return None
    if not reply or not isinstance(reply.get("kiosk"), dict):
        return None
    return reply["kiosk"]


def kiosk_policy_problems(state):
    """Reasons this kiosk cannot deliver the screen the product promises.

    Empty list = healthy. `waiting_for_game_package` is NOT a problem: the
    setting is written moments later by assert_kiosk_game and the kiosk's
    ContentObserver picks it up, so reporting it would fail every launch that
    is merely early.
    """
    if state is None:
        return ["the kiosk did not answer a status broadcast"]
    if not state.get("activity_created"):
        # A STATUS broadcast can cold-start the kiosk's PROCESS without
        # MainActivity having run; that record is blank, not broken.
        return ["the kiosk activity has not started (it is not HOME)"]
    problems = [k for k in KIOSK_REQUIRED_POLICY if not state.get(k)]
    failures = state.get("policy_failures")
    if failures:
        problems.append(f"policy_failures={failures}")
    if state.get("status") == "launch_failed":
        problems.append(
            f"the kiosk gave up launching after "
            f"{state.get('launch_attempts')} attempts "
            f"({state.get('last_error')})")
    return problems


def assert_kiosk_ready(acct, label):
    """Ask the kiosk whether it is healthy, and SAY SO when it is not.

    The failure this exists for: every device-owner call in MainActivity used
    to be a catch-and-log, so a setLockTaskPackages that did not apply
    produced a booted, reachable, completely unplayable instance and a
    successful-looking `omnidroid start`. Reported, not raised — the instance
    is up and the caller decides what to do about it.
    """
    if not kiosk_installed(acct):
        print(f"[{label}] kiosk health: NOT INSTALLED — the session/auto-join "
              f"feature needs it (omnidroid kioskify {acct.get('name')})")
        return {"ready": False, "state": None, "problems": ["kiosk_missing"]}
    try:
        state = kiosk_status(acct)
    except Exception as e:  # noqa: BLE001 — never raise into a boot path
        print(f"[{label}] kiosk health: could not be read ({e!r})")
        return {"ready": False, "state": None,
                "problems": [f"status_error: {e!r}"]}
    problems = kiosk_policy_problems(state)
    if problems:
        print(f"[{label}] kiosk health: *** NOT CLEAN *** — "
              + "; ".join(str(p) for p in problems))
    else:
        print(f"[{label}] kiosk health: device owner, lock task pinned, "
              f"status bar off, game = {state.get('game_package')}")
    return {"ready": not problems, "state": state, "problems": problems}
```

At `engine.py:10300`, replace `assert_kiosk_game(acct, cfg, label)` with:

```python
    assert_kiosk_game(acct, cfg, label)
    # ...and then CHECK, rather than assume, that the kiosk applied the policy
    # the screen depends on. Reported into the boot log; `omnidroid start`
    # carries it in its result so a launch can fail loudly instead of handing
    # back a black screen that looks successful.
    kiosk_health = assert_kiosk_ready(acct, label)
```

- [ ] **Step 4: Run the tests**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_kiosk_boot_app.py -q
```
Expected: PASS.

- [ ] **Step 5: Run the full suite and diff against the baseline**

```bash
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | grep '^FAILED' | sort > /tmp/after.txt
```
Expected: `/tmp/after.txt` contains the same 11 baseline `FAILED` lines and no others. Adding names to `engine` is safe — `tests/test_facade_equivalence.py` only checks for names that went MISSING.

- [ ] **Step 6: Ship the APK into both bases and verify end to end**

x86 (the /system swap builds a NEW base version — run it on the build host):
```bash
omnidroid update-kiosk launcher/build/omni-kiosk.apk
```
ARM (refreshes the /data template; run over ssh to the Mac, `--no-window` is mandatory):
```bash
ssh 192.168.0.30 'cd ~/omnidroid && ./launcher/build.sh && \
  omnidroid update-kiosk launcher/build/omni-kiosk.apk --base arm --no-window'
```
Then, on each base:
```bash
omnidroid start u1                     # x86
ssh 192.168.0.30 'omnidroid start u1 --no-window'   # arm
```
Expected in both logs: `kiosk health: device owner, lock task pinned, status bar off, game = com.roblox.client`.

- [ ] **Step 7: Commit**

```bash
git add omnidroid/engine.py tests/test_kiosk_boot_app.py
git commit -m "feat(engine): fail a launch loudly when the kiosk policy did not apply"
```

---

### Task 6: The pre-session force-stop is verified, not assumed

`SessionReceiver` deliberately does not kill Roblox (lines ~256-262) and the reasoning is correct — the kiosk is an ordinary app and `killBackgroundProcesses()` no-ops on a FOREGROUND Roblox, i.e. it fails exactly when switching accounts. The host owns the step: `_deliver_session` runs `am force-stop com.roblox.client` at `engine.py:8367`.

Two things to establish. First, that it is unconditional: `restart=True` is the default and both call sites (`engine.py:2104` and `engine.py:11033`) use the default — pin that so nobody adds a third that does not. Second, that it **worked**: a force-stop that fails is currently swallowed by `except Exception`, and a Roblox that survives joins as the previous account with the previous cookie — silently.

**Files:**
- Modify: `omnidroid/engine.py:8360-8372` (the restart block inside `_deliver_session`)
- Test: `tests/test_session.py` (append; the file currently ends after `ApkBootstrapLoginProbe`)

**Interfaces:**
- Consumes: existing `engine.adb`, `engine.adb_soft`, `engine.ROBLOX_PACKAGE`.
- Produces: `engine.force_stop_verified(acct, pkg, label, tries=3) -> bool`. `_deliver_session`'s status dict gains `stale_client: bool`; when True it also sets `reason = "stale_client"`.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_session.py`:

```python
class TheGameIsReallyStoppedBeforeAHandOff(unittest.TestCase):
    """A cookie swap only takes effect on a COLD Roblox process — the client
    caches the authenticated user at startup. A force-stop that did not work
    means the next join is as the PREVIOUS account, and nothing said so."""

    def _acct(self):
        return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
                "vnc_port": 18001, "base": "arm"}

    def test_a_dead_process_verifies_first_time(self):
        with mock.patch.object(omni, "adb") as adb, \
             mock.patch.object(omni, "adb_soft",
                               return_value=SimpleNamespace(
                                   returncode=0, stdout="", stderr="")):
            ok = omni.force_stop_verified(self._acct(), "com.roblox.client", "t")
        self.assertTrue(ok)
        flat = " ".join(str(c) for c in adb.call_args_list)
        self.assertIn("force-stop com.roblox.client", flat.replace("', '", " "))

    def test_a_surviving_process_is_retried_then_reported(self):
        with mock.patch.object(omni, "adb"), \
             mock.patch.object(omni, "adb_soft",
                               return_value=SimpleNamespace(
                                   returncode=0, stdout="4211\n", stderr="")):
            ok = omni.force_stop_verified(self._acct(), "com.roblox.client",
                                          "t", tries=3)
        self.assertFalse(ok)

    def test_it_stops_as_soon_as_the_process_is_gone(self):
        answers = [SimpleNamespace(returncode=0, stdout="4211\n", stderr=""),
                   SimpleNamespace(returncode=0, stdout="", stderr="")]
        with mock.patch.object(omni, "adb") as adb, \
             mock.patch.object(omni, "adb_soft", side_effect=answers):
            ok = omni.force_stop_verified(self._acct(), "com.roblox.client",
                                          "t", tries=5)
        self.assertTrue(ok)
        self.assertEqual(adb.call_count, 2)

    def test_an_unanswered_probe_is_not_read_as_stopped(self):
        # adb_soft answers returncode -1 with empty stdout when it could not
        # complete. Treating that as "no pid, so it stopped" would be exactly
        # the silent wrong-account join this function exists to catch.
        with mock.patch.object(omni, "adb"), \
             mock.patch.object(omni, "adb_soft",
                               return_value=SimpleNamespace(
                                   returncode=-1, stdout="",
                                   stderr="<no answer within 15s>")):
            ok = omni.force_stop_verified(self._acct(), "com.roblox.client",
                                          "t", tries=2)
        self.assertFalse(ok)


class TheForceStopIsUnconditional(unittest.TestCase):
    def test_restart_defaults_to_true(self):
        import inspect
        sig = inspect.signature(omni._deliver_session)
        self.assertIs(sig.parameters["restart"].default, True)

    def test_every_caller_uses_the_default(self):
        import inspect
        src = inspect.getsource(omni)
        self.assertNotIn("restart=False", src,
                         "a caller that skips the force-stop joins as the "
                         "previous account")

    def test_a_stale_client_is_reported_in_the_status(self):
        import inspect
        src = inspect.getsource(omni._deliver_session)
        self.assertIn("stale_client", src)
        self.assertIn("force_stop_verified(", src)
```

Ensure `tests/test_session.py` imports `SimpleNamespace` — it already does (line 17).

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_session.py -q -k "TheGameIsReallyStopped or TheForceStopIsUnconditional"
```
Expected: FAIL with `AttributeError: module 'omnidroid.engine' has no attribute 'force_stop_verified'`.

- [ ] **Step 3: Implement the verified force-stop**

Add to `omnidroid/engine.py` immediately above `def deliver_session(...)`:

```python
def force_stop_verified(acct, pkg, label, tries=3):
    """`am force-stop pkg`, then CHECK the process is actually gone.

    The failure this exists for is silent by construction. A cookie swap only
    takes effect on a COLD Roblox process — the client reads its cookie jar
    during startup and caches the authenticated user — so a force-stop that
    did not land produces a join as the PREVIOUS account with the previous
    token, on an instance that reports a successful launch. The old code
    swallowed the force-stop's exception and never looked.

    `pidof` through adb_soft, so a slow guest answers late rather than
    raising. An UNANSWERED probe (returncode -1) is NOT read as "no pid": that
    would turn "adb went away" into "it stopped", which is the exact wrong
    answer. Returns True only when a probe actually came back empty.
    """
    for n in range(1, tries + 1):
        try:
            adb(acct, "shell", "am", "force-stop", pkg, timeout=25)
        except Exception as e:  # noqa: BLE001 — a live instance still plays
            print(f"[{label}] could not force-stop {pkg}: {e}")
        r = adb_soft(acct, "shell", "pidof", pkg, timeout=15)
        if r.returncode == -1:
            print(f"[{label}] could not confirm {pkg} stopped "
                  f"(the guest did not answer)")
            continue
        if not (r.stdout or "").strip():
            return True
        print(f"[{label}] {pkg} is STILL RUNNING after force-stop "
              f"(attempt {n}/{tries}) — a live process would join as the "
              f"previous account")
    return False
```

Replace the restart block in `_deliver_session` (lines 8360-8372):

```python
    stale = False
    if restart:
        # Cold-start Roblox so the new cookie is read at startup. Without this,
        # switching accounts silently joins as the PREVIOUS user: the client
        # caches the authenticated user in-process. This has to happen host-side
        # — as shell we hold FORCE_STOP_PACKAGES, whereas the kiosk (an ordinary
        # app, device owner or not) cannot stop a foreground app at all. Applies
        # in HOME mode too (play=False): the cookie still needs a cold start to
        # take effect.
        #
        # VERIFIED, not fired-and-forgotten: a force-stop that did not work is
        # indistinguishable from one that did, right up until the instance
        # farms on somebody else's account.
        stale = not force_stop_verified(acct, ROBLOX_PACKAGE, label)
```

And in the status dict, after `status = {...}`:

```python
    status["stale_client"] = stale
    if stale:
        status["reason"] = "stale_client"
        print(f"[{label}] *** the previous {ROBLOX_PACKAGE} process survived "
              f"the force-stop; this session may join as the previous "
              f"account ***")
```

- [ ] **Step 4: Run the tests**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_session.py -q
```
Expected: PASS.

- [ ] **Step 5: Verify on a live instance**

```bash
omnidroid start u1
adb -s 127.0.0.1:16001 shell pidof com.roblox.client   # note the pid
omnidroid play u1
adb -s 127.0.0.1:16001 shell pidof com.roblox.client   # must be a NEW pid
```
Expected: a different pid, and no `*** the previous com.roblox.client process survived` line in the `omnidroid play` output.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/engine.py tests/test_session.py
git commit -m "fix(session): verify the pre-hand-off force-stop instead of assuming it"
```

---

### Task 7: Full-disk access gets a root fallback and a read-back

`consent.py:77-99` already issues `appops set <pkg> MANAGE_EXTERNAL_STORAGE allow` plus `pm grant` for every dangerous permission each installed package declares, using shell's `MANAGE_APP_OPS_MODES` — root not required — and `engine.apply_consent` already reads the state back with `build_consent_probe`. The measured note in `consent.py:29-32` is explicit that none of it needs root, and it is re-applied every boot because app-ops do not survive ephemeral `/data`.

What is missing is the case where shell does **not** hold `MANAGE_APP_OPS_MODES` — a host whose adbd is not uid 0 and whose base was built differently. Today that reads back as `full_disk: False`, prints "NOTHING landed", and stops. This task adds one repair pass through `engine.root_shell`, then re-probes, so the report still comes from a read-back and never from having sent a command.

**Files:**
- Modify: `omnidroid/consent.py:55` (imports), and append `build_appops_repair`
- Modify: `omnidroid/engine.py:9154-9202` (`apply_consent`)
- Test: `tests/test_consent.py` (append)

**Interfaces:**
- Consumes: existing `consent.APP_OPS`, `consent.build_consent_probe(game_pkg)`, `consent.parse_consent_state(text)`, `consent.summary_line(counts, state)`; existing `engine.resolve_root_shell(acct)` (returns `""`, a `su` path, or `None`) and `engine.root_shell(acct, script, timeout=30)`.
- Produces: `consent.build_appops_repair(game_pkg, su=None) -> list[str] | None` — an adb argv vector, or `None` when `su is None`. `engine.apply_consent` gains no new signature.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_consent.py`:

```python
class TheRootFallbackForFullDiskAccess(unittest.TestCase):
    """appops needs MANAGE_APP_OPS_MODES, which uid shell holds on both
    shipped bases. On a deployment where it does not, the whole 'full disk
    access' promise silently reads back as `default` — so there is one repair
    pass as root, and then the state is read AGAIN."""

    def test_no_root_means_no_repair_script_rather_than_a_pretend_one(self):
        self.assertIsNone(consent.build_appops_repair(GAME, su=None))

    def test_an_empty_string_is_a_VALID_root_mode(self):
        # "" means adbd ITSELF runs as uid 0 — exactly the x86 Bliss base.
        # `if not su:` read that as "no root" and skipped the step; every
        # gate has to be an identity test against None.
        step = consent.build_appops_repair(GAME, su="")
        self.assertIsNotNone(step)
        self.assertNotIn(" 0 sh -c", _script(step))

    def test_a_su_path_is_prefixed(self):
        step = consent.build_appops_repair(GAME, su="/debug_ramdisk/su")
        self.assertIn("/debug_ramdisk/su 0 sh -c", " ".join(step))

    def test_the_repair_covers_the_op_the_feature_is_named_after(self):
        step = consent.build_appops_repair(GAME, su="")
        self.assertIn("MANAGE_EXTERNAL_STORAGE", _script(step))

    def test_the_repair_is_quoted_as_one_argument(self):
        step = consent.build_appops_repair(GAME, su="")
        self.assertTrue(_script(step).startswith(("'", '"')),
                        f"not quoted: {_script(step)[:60]}")

    def test_the_repair_names_the_package_explicitly(self):
        step = consent.build_appops_repair(GAME, su="")
        self.assertIn(GAME, _script(step))
```

And, in the same file, the engine-side wiring:

```python
class TheEngineRepairsAndThenRereads(unittest.TestCase):
    """`appops set` on a package that does not exist exits 0 and does
    nothing. Nothing here may report success on the strength of a command
    having been sent."""

    def setUp(self):
        import omnidroid.engine as omni
        self.omni = omni
        self.acct = {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
                     "vnc_port": 18001, "base": "x86"}

    def test_a_failed_op_is_retried_as_root_and_reprobed(self):
        from unittest import mock
        from types import SimpleNamespace
        # First probe: the op did NOT apply. Second (after the repair): it did.
        probes = [
            "hide_error_dialogs=1\nfull_disk=default\ninstall_unknown=allow",
            "hide_error_dialogs=1\nfull_disk=allow\ninstall_unknown=allow",
        ]
        seen = {"repairs": 0}

        def fake_adb(acct, *args, **kw):
            script = args[-1]
            if "appops get" in script:
                return SimpleNamespace(returncode=0, stdout=probes.pop(0),
                                       stderr="")
            # The repair is the ONLY script that sets ops without also
            # granting runtime permissions.
            if "appops set" in script and "pm grant" not in script:
                seen["repairs"] += 1
            return SimpleNamespace(
                returncode=0,
                stdout=f"{consent.COUNTS_PREFIX} 3 12 6 18\n{consent.CONSENT_OK}",
                stderr="")

        with mock.patch.object(self.omni, "adb", side_effect=fake_adb), \
             mock.patch.object(self.omni, "resolve_root_shell", return_value=""), \
             mock.patch.object(self.omni, "resolve_game_package",
                               return_value=GAME):
            ok = self.omni.apply_consent(self.acct, {}, "t")
        self.assertTrue(ok)
        self.assertEqual(seen["repairs"], 1, "the repair must run exactly once")
        self.assertEqual(probes, [], "the state must be read back AFTER repair")

    def test_no_root_available_means_no_repair_and_an_honest_report(self):
        from unittest import mock
        from types import SimpleNamespace

        def fake_adb(acct, *args, **kw):
            script = args[-1]
            if "appops get" in script:
                return SimpleNamespace(
                    returncode=0,
                    stdout="hide_error_dialogs=1\nfull_disk=default\n"
                           "install_unknown=default",
                    stderr="")
            return SimpleNamespace(
                returncode=0,
                stdout=f"{consent.COUNTS_PREFIX} 3 12 6 18\n{consent.CONSENT_OK}",
                stderr="")

        with mock.patch.object(self.omni, "adb", side_effect=fake_adb), \
             mock.patch.object(self.omni, "resolve_root_shell",
                               return_value=None), \
             mock.patch.object(self.omni, "root_shell") as rs, \
             mock.patch.object(self.omni, "resolve_game_package",
                               return_value=GAME):
            self.omni.apply_consent(self.acct, {}, "t")
        rs.assert_not_called()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_consent.py -q -k "RootFallback or RepairsAndThenRereads"
```
Expected: FAIL with `AttributeError: module 'omnidroid.consent' has no attribute 'build_appops_repair'`.

- [ ] **Step 3: Add the repair builder to consent.py**

Append to `omnidroid/consent.py` (after `build_consent_probe`):

```python
def build_appops_repair(game_pkg, su=None):
    """Re-issue the app-ops as ROOT, for a host whose shell was refused.

    `appops set` needs MANAGE_APP_OPS_MODES, which uid shell holds on both
    shipped bases (MEASURED — see the module docstring). This is the fallback
    for a deployment where it does not: adbd not uid 0, no `adb root`, and the
    ops read back as `default` afterwards.

    `su is None`, NOT `not su`. "" is a VALID root mode — it means adbd itself
    runs as uid 0, which is exactly what the x86 Bliss base does — and a falsy
    test read it as "no root" and skipped the step. That trap is documented on
    gaming.su_sh and cost months of a silently-disabled cpuset step; every gate
    on a root mode has to be an identity test against None.

    Returns None when there is no root to fall back to, so the caller reports
    the op as NOT applied instead of pretending it retried.
    """
    if su is None:
        return None
    ops = " ".join(APP_OPS)
    script = (f"for OP in {ops}; do "
              f"appops set {shlex.quote(game_pkg)} $OP allow "
              f">/dev/null 2>&1; done; true")
    if su == "":
        return sh(script)
    return ["shell", "sh", "-c",
            shlex.quote(f"{su} 0 sh -c {shlex.quote(script)}")]
```

- [ ] **Step 4: Wire it into apply_consent**

In `omnidroid/engine.py`, inside `apply_consent`, replace the read-back block (the `state = {...}` / `if game:` section at lines ~9192-9200) with:

```python
    # Read the state BACK rather than trusting the commands that were sent:
    # `appops set` on a package that does not exist exits 0 and does nothing.
    state = {"dialogs_hidden": True, "full_disk": False, "install_unknown": False}
    if game:
        try:
            r = adb(acct, *consent.build_consent_probe(game), timeout=30)
            state = consent.parse_consent_state((r.stdout or "")
                                                + (r.stderr or ""))
        except Exception:       # noqa: BLE001 — probe failure is not a failure
            pass
        # ROOT FALLBACK. The ops need MANAGE_APP_OPS_MODES, which uid shell
        # holds on both shipped bases; on a deployment where it does not, the
        # whole "full disk access" promise reads back as `default`. One repair
        # pass as root, then read the state AGAIN — never report on the
        # strength of having retried.
        # `su is None` — NEVER `not su`. "" is a valid root mode (x86 Bliss
        # adbd runs as uid 0); build_appops_repair encodes that rule and
        # returns None only when there is genuinely no root.
        if not state.get("full_disk"):
            su = resolve_root_shell(acct)
            step = consent.build_appops_repair(game, su)
            if step is None:
                if label:
                    print(f"[{label}] consent: full disk access did not apply "
                          f"and this guest offers no root to retry as")
            else:
                # build_appops_repair returns a COMPLETE adb argv vector with
                # the su prefix already baked in, so the repair goes through
                # `adb`; root_shell is not involved on this path.
                try:
                    adb(acct, *step, timeout=60)
                except Exception as e:  # noqa: BLE001
                    if label:
                        print(f"[{label}] consent: root repair failed ({e})")
                try:
                    r = adb(acct, *consent.build_consent_probe(game),
                            timeout=30)
                    state = consent.parse_consent_state((r.stdout or "")
                                                        + (r.stderr or ""))
                except Exception:  # noqa: BLE001
                    pass
```

The `rs.assert_not_called()` in `test_no_root_available_means_no_repair_and_an_honest_report` stays as written: it is the guard that this path never reaches for `root_shell`, and it must keep passing.

- [ ] **Step 5: Run the tests**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_consent.py -q
```
Expected: PASS, including the six pre-existing classes.

- [ ] **Step 6: Verify the read-back on both bases**

```bash
omnidroid start u1
adb -s 127.0.0.1:16001 shell appops get com.roblox.client MANAGE_EXTERNAL_STORAGE
```
Expected: `MANAGE_EXTERNAL_STORAGE: allow`, and the boot log line `consent: full disk access, install-unknown, error dialogs off (N package(s), M runtime permission(s) granted)`.

Same on ARM:
```bash
ssh 192.168.0.30 'omnidroid start u1 --no-window'
ssh 192.168.0.30 'adb -s 127.0.0.1:16001 shell appops get com.roblox.client MANAGE_EXTERNAL_STORAGE'
```
Expected: `allow`.

- [ ] **Step 7: Commit**

```bash
git add omnidroid/consent.py omnidroid/engine.py tests/test_consent.py
git commit -m "feat(consent): root fallback for full-disk access, verified by read-back"
```

---

### Task 8: Readiness polls replace the fixed sleeps in the boot path

Three fixed sleeps sit in the provisioning path and each one is either too long or too short depending on the guest:
- `engine.py:835` — `time.sleep(3)` after `adb root` in `post_boot`
- `engine.py:848` — `time.sleep(2)` after `adb root` in `provision_settings`
- `engine.py:878` — `time.sleep(3)` after `am start` of the kiosk in `provision_settings`

A squeezed farming guest answers adb far slower than 3 s (that is why `adb_soft` and the 120 s broadcast budget exist); a warm x86 guest is ready in well under it. Both cases are wrong, and the `am start` one is the one that produces an unbaked wallpaper.

**Files:**
- Modify: `omnidroid/engine.py:820-840` (`post_boot`), `:842-880` (`provision_settings`)
- Test: `tests/test_start_timings.py` (append)

**Interfaces:**
- Consumes: existing `engine.adb_connect(acct)`, `engine.adb_soft(acct, *args, timeout=...)`, `engine.adb_state(acct)`.
- Produces:
  - `engine.poll_until(fn, timeout, interval=0.25) -> bool` — calls `fn()` until it answers truthy or `timeout` seconds elapse; a raising `fn` counts as a falsy answer.
  - `engine.wait_adb_ready(acct, timeout=20) -> bool` — endpoint answers `device` and `adb shell echo ok` prints `ok`.
  - `engine.wait_for_package_process(acct, pkg, timeout=20) -> bool` — `pidof pkg` is non-empty.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_start_timings.py`:

```python
class PollingBeatsSleeping(unittest.TestCase):
    """A squeezed farming guest answers adb far slower than 3 s; a warm x86
    guest is ready in well under it. A fixed sleep is wrong in both
    directions, and the one after `am start` is what left the black
    wallpaper unbaked."""

    def test_it_returns_as_soon_as_the_condition_holds(self):
        calls = []

        def fn():
            calls.append(1)
            return len(calls) >= 3

        self.assertTrue(omni.poll_until(fn, timeout=5, interval=0.01))
        self.assertEqual(len(calls), 3)

    def test_it_gives_up_and_says_so(self):
        self.assertFalse(omni.poll_until(lambda: False, timeout=0.05,
                                         interval=0.01))

    def test_a_raising_probe_is_a_falsy_answer_not_a_crash(self):
        def boom():
            raise RuntimeError("adb went away")
        self.assertFalse(omni.poll_until(boom, timeout=0.05, interval=0.01))

    def test_it_always_probes_at_least_once(self):
        calls = []
        omni.poll_until(lambda: calls.append(1) or False, timeout=0.0,
                        interval=0.01)
        self.assertEqual(len(calls), 1)


class TheAdbReadinessProbe(unittest.TestCase):
    def _acct(self):
        return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
                "vnc_port": 18001, "base": "x86"}

    def test_a_live_endpoint_is_ready(self):
        with mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "adb_state", return_value="device"), \
             mock.patch.object(omni, "adb_soft",
                               return_value=SimpleNamespace(
                                   returncode=0, stdout="ok\n", stderr="")):
            self.assertTrue(omni.wait_adb_ready(self._acct(), timeout=1))

    def test_an_offline_endpoint_is_not_ready(self):
        with mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "adb_state", return_value="offline"), \
             mock.patch.object(omni, "adb_soft",
                               return_value=SimpleNamespace(
                                   returncode=-1, stdout="", stderr="")):
            self.assertFalse(omni.wait_adb_ready(self._acct(), timeout=0.05))


class TheProvisioningPathNoLongerSleepsBlind(unittest.TestCase):
    def test_post_boot_polls_instead_of_sleeping(self):
        import inspect
        src = inspect.getsource(omni.post_boot)
        self.assertNotIn("time.sleep(", src)
        self.assertIn("wait_adb_ready(", src)

    def test_provision_settings_polls_instead_of_sleeping(self):
        import inspect
        src = inspect.getsource(omni.provision_settings)
        self.assertNotIn("time.sleep(", src)
        self.assertIn("wait_adb_ready(", src)
        # The kiosk `am start` exists to bake the black wallpaper into /data;
        # continuing before its process is up captures nothing.
        self.assertIn("wait_for_package_process(", src)
```

Ensure `tests/test_start_timings.py` has `from types import SimpleNamespace` and `from unittest import mock` at the top; add whichever is absent.

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_start_timings.py -q -k "PollingBeats or TheAdbReadiness or NoLongerSleepsBlind"
```
Expected: FAIL with `AttributeError: module 'omnidroid.engine' has no attribute 'poll_until'`.

- [ ] **Step 3: Add the poll helpers**

Add to `omnidroid/engine.py` immediately above `def post_boot(...)` (line 820):

```python
def poll_until(fn, timeout, interval=0.25):
    """Call `fn()` until it answers truthy, or `timeout` seconds pass.

    Always probes at least once, so `timeout=0` means "check now" rather than
    "never check". A probe that RAISES counts as a falsy answer: these run
    against a guest that is by definition not ready yet, and an adb call that
    dies mid-boot is the ordinary case, not an error.
    """
    deadline = time.time() + max(0.0, timeout)
    while True:
        try:
            if fn():
                return True
        except Exception:  # noqa: BLE001 — a probe must never raise
            pass
        if time.time() >= deadline:
            return False
        time.sleep(interval)


def wait_adb_ready(acct, timeout=20):
    """Poll until the adb endpoint answers a shell command.

    Replaces the fixed `time.sleep(3)` after `adb root`: adbd RESTARTS as uid
    0, so the endpoint drops and comes back, and how long that takes is a
    property of the guest. Three seconds is far too long on a warm x86 guest
    and far too short on a squeezed farming one — the same asymmetry adb_soft
    and the 120 s broadcast budget exist for.
    """
    def ready():
        adb_connect(acct)
        if adb_state(acct) != "device":
            return False
        r = adb_soft(acct, "shell", "echo", "ok", timeout=8)
        return "ok" in (r.stdout or "")

    return poll_until(ready, timeout)


def wait_for_package_process(acct, pkg, timeout=20):
    """Poll until `pkg` has a live process in the guest.

    Replaces the fixed `time.sleep(3)` after `am start` of the kiosk: that
    start exists to bake the solid-black wallpaper into /data before the first
    production boot, and continuing before the process is even up captures an
    image with the vendor wallpaper still in it.
    """
    def up():
        r = adb_soft(acct, "shell", "pidof", pkg, timeout=10)
        return bool((r.stdout or "").strip())

    return poll_until(up, timeout)
```

- [ ] **Step 4: Replace the three sleeps**

In `post_boot`, replace lines 834-836:

```python
    adb(acct, "root")
    if not wait_adb_ready(acct, timeout=25):
        print(f"[{label}] adb did not come back after `adb root`")
    adb_connect(acct)
```

In `provision_settings`, replace lines 847-849:

```python
    adb(acct, "root")
    if not wait_adb_ready(acct, timeout=25):
        print(f"[{label}] adb did not come back after `adb root`")
    adb_connect(acct)
```

In `provision_settings`, replace lines 875-878 (the kiosk `am start` + sleep):

```python
        # Launch the kiosk once now so it sets the solid-black wallpaper
        # into /data before the first production boot (no wallpaper flash).
        adb(acct, "shell", "am", "start", "-n",
            "com.omni.kiosk/.MainActivity", timeout=15)
        if not wait_for_package_process(acct, "com.omni.kiosk", timeout=30):
            print(f"[{label}] the kiosk process did not come up; the black "
                  f"wallpaper may not be baked into this /data")
```

- [ ] **Step 5: Run the tests and the full suite**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_start_timings.py -q
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | grep '^FAILED' | sort
```
Expected: the target file passes; the full-suite `FAILED` list matches the 11 baseline lines exactly.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/engine.py tests/test_start_timings.py
git commit -m "perf(boot): poll for adb and kiosk readiness instead of fixed sleeps"
```

---

### Task 9: The silent-boot checklist, per base

Per spec §6: *"most of this already exists and needs verifying on both bases rather than building… The deliverable is a checked list per base, not new code — plus whatever the check finds missing."* This task builds the machine-checkable half (invariants that a future edit cannot quietly break) and produces the dated, filled-in checklist.

What already exists, and where — **this inventory is the starting state, and every line of it must be confirmed present before anything is added**:

| Cosmetic | x86 (Bliss) | ARM (LineageOS) |
|---|---|---|
| Quiet kernel cmdline | `qemu_proc.py:1834-1835` — `quiet loglevel=0 console=null vt.global_cursor_default=0 SETUPWIZARD=0`, in the **non-interactive** branch only (interactive gets `console=tty0 console=ttyS0,115200` at `:1829`) | `engine.SILENT_CMDLINE` at `engine.py:3337-3338` |
| Bootloader menu | n/a (direct kernel boot, no GRUB) | `engine.GRUB_NO_MENU` at `:3340-3348` forcing `set timeout=0` / `set timeout_style=hidden`, applied by `_silence_boot` (`:3392`) |
| Boot animation | baked into `/system/media/bootanimation.zip` at base `v2` | `assets/loading/bootanimation.zip` via `_replace_bootanimation` (`:3227`) — see `LOADING-SCREEN.md` |
| Status/nav bar | `MainActivity.hideSystemUi()` (~288), device-owner `setStatusBarDisabled(true)` + `setLockTaskFeatures(LOCK_TASK_FEATURE_NONE)` (~262-267), `settings put secure immersive_mode_confirmations confirmed` (`engine.py:857-858`) | same (one APK, both bases) |
| Lock screen | `dismissKeyguard()` three layers (~187-211), `res/xml/device_admin.xml` `<disable-keyguard/>`, host `locksettings set-disabled true` + `settings put secure lockscreen.disabled 1` (`engine.py:851-852`) | same |
| Wallpaper | `ensureBlackWallpaper()` 1x1 black bitmap (~216-226), baked at provision time (`engine.py:874-880`) | same |
| Host console flashes | `config.install_no_console_default()` (`config.py:179-212`), guarded by `tests/test_no_console_windows.py` | n/a (no Windows console on macOS) |

**Files:**
- Create: `omnidroid/silentboot.py`
- Create: `tests/test_silent_boot.py`
- Create: `docs/superpowers/runbooks/2026-08-19-C-silent-boot-checklist.md`
- Modify: `omnidroid/engine.py` (one call in the shared boot tail, next to `assert_kiosk_ready`)

**Interfaces:**
- Consumes: `engine.SILENT_CMDLINE`, `engine.GRUB_NO_MENU` (existing module constants); `qemu_proc.qemu_command(acct, cfg, interactive, ...)` (existing); `engine.adb`, `engine.KIOSK_PACKAGE`, `engine.assert_kiosk_ready` (Task 5).
- Produces:
  - `silentboot.X86_REQUIRED_CMDLINE: tuple[str, ...]`, `silentboot.X86_FORBIDDEN_CMDLINE: tuple[str, ...]`, `silentboot.ARM_REQUIRED_CMDLINE: tuple[str, ...]`, `silentboot.ARM_REQUIRED_GRUB: tuple[str, ...]`
  - `silentboot.cmdline_problems(append: str, arm: bool) -> list[str]`
  - `silentboot.grub_problems(grub_cfg: str) -> list[str]`
  - `silentboot.build_boot_face_probe() -> list[str]` (an adb argv vector)
  - `silentboot.parse_boot_face(text: str) -> dict` with keys `lock_task`, `lock_task_packages`, `immersive_confirmed`, `lockscreen_disabled`, `home_activity`, `bootanimation`
  - `silentboot.boot_face_problems(state: dict) -> list[str]`
  - `silentboot.summary_line(state: dict) -> str`
  - `engine.report_boot_face(acct, label) -> dict`

- [ ] **Step 1: Write the failing test**

Create `tests/test_silent_boot.py`:

```python
#!/usr/bin/env python3
"""The boot has no face until the game has one.

    python3 tests/test_silent_boot.py

Nearly all of this already exists (see the plan's inventory table). These
tests are the ratchet: they turn "we checked once, by eye, on one base" into
an assertion that a later edit cannot quietly undo.

The x86 one is the sharp one. The production and the interactive/builder
boots build their kernel cmdline in the SAME function, three lines apart
(qemu_proc.py:1829 vs :1834). An edit that moves `console=ttyS0,115200` out
of the interactive branch puts a scrolling kernel log on a customer's screen,
and nothing else in the suite would notice.
"""
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402
from omnidroid import silentboot  # noqa: E402


def _cfg():
    return {
        "images_dir": "/imgs",
        "data_template": "x86/data-template-8g.qcow2",
        "qemu": {"smp": 4, "mem_mb": 4096},
        "bases": {
            "x86": {"type": "x86-bliss", "disk": "x86/base_x86.qcow2",
                    "kernel": "x86/base_x86.kernel",
                    "initrd": "x86/base_x86.initrd.img",
                    "src": "/android-2024-10-11"},
        },
    }


def _acct(**over):
    a = {"name": "u1", "base": "x86", "adb_port": 16001, "qmp_port": 17001,
         "vnc_port": 18001, "ephemeral": True}
    a.update(over)
    return a


def _x86_append(interactive):
    """The real -append string qemu_command() builds for this boot."""
    with mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda x: x), \
         mock.patch.object(qemu_proc, "default_accel", return_value="whpx"), \
         mock.patch.object(qemu_proc, "resolve_mode",
                           return_value={"smp": 4, "mem": 4096,
                                         "name": "playable"}), \
         mock.patch.object(qemu_proc, "_assert_port_triple", return_value=1), \
         mock.patch.object(qemu_proc, "resolve_gpu_display",
                           return_value=([], ["-display", "none"])), \
         mock.patch.object(qemu_proc, "usb_devices", return_value=[]), \
         mock.patch.object(qemu_proc, "balloon_device", return_value=[]), \
         mock.patch.object(qemu_proc, "devkit_drive_args", return_value=[]), \
         mock.patch("omnidroid.engine.runtime_dir",
                    side_effect=lambda n: Path(f"/RT/{n}")), \
         mock.patch("omnidroid.engine.account_dir",
                    side_effect=lambda n: Path(f"/ACC/{n}")):
        cmd = qemu_proc.qemu_command(_acct(), _cfg(), interactive)
    return cmd[cmd.index("-append") + 1]


class TheX86ProductionBootIsSilent(unittest.TestCase):
    def test_the_production_cmdline_carries_every_quiet_flag(self):
        self.assertEqual(
            silentboot.cmdline_problems(_x86_append(False), arm=False), [])

    def test_production_never_gets_a_console(self):
        append = _x86_append(False)
        for bad in silentboot.X86_FORBIDDEN_CMDLINE:
            self.assertNotIn(bad, append)

    def test_the_interactive_boot_still_has_its_serial_log(self):
        # The debuggability of a builder boot is not the thing being removed.
        self.assertIn("console=ttyS0,115200", _x86_append(True))

    def test_a_console_leaking_into_production_is_caught(self):
        probs = silentboot.cmdline_problems(
            "quiet loglevel=0 console=null vt.global_cursor_default=0 "
            "SETUPWIZARD=0 console=tty0", arm=False)
        self.assertTrue(any("console=tty0" in p for p in probs), probs)


class TheArmBootIsSilent(unittest.TestCase):
    def test_the_shipped_cmdline_carries_every_quiet_flag(self):
        self.assertEqual(
            silentboot.cmdline_problems(omni.SILENT_CMDLINE, arm=True), [])

    def test_grub_shows_no_menu_and_no_countdown(self):
        self.assertEqual(silentboot.grub_problems(omni.GRUB_NO_MENU), [])

    def test_a_visible_grub_menu_is_caught(self):
        probs = silentboot.grub_problems(
            "set timeout=10\nset timeout_style=menu\n")
        self.assertTrue(probs)


class TheGuestProbe(unittest.TestCase):
    def test_it_is_quoted_as_one_argument(self):
        step = silentboot.build_boot_face_probe()
        self.assertEqual(step[:3], ["shell", "sh", "-c"])
        self.assertEqual(len(step), 4)
        self.assertTrue(step[-1].startswith(("'", '"')))

    def test_it_asks_about_every_surface_the_user_named(self):
        script = silentboot.build_boot_face_probe()[-1]
        for probe in ("mLockTaskModeState", "immersive_mode_confirmations",
                      "lockscreen.disabled", "bootanimation.zip"):
            self.assertIn(probe, script)

    def test_a_clean_boot_parses_clean(self):
        out = (
            "lock_task=mLockTaskModeState=LOCKED\n"
            "lock_task_packages=mLockTaskPackages(userId:packages)=0:"
            "com.omni.kiosk,com.roblox.client\n"
            "immersive=confirmed\n"
            "lockscreen_disabled=1\n"
            "home_activity=com.omni.kiosk/.MainActivity\n"
            "bootanimation=/system/media/bootanimation.zip\n")
        state = silentboot.parse_boot_face(out)
        self.assertEqual(silentboot.boot_face_problems(state), [])

    def test_an_unpinned_kiosk_is_a_problem(self):
        state = silentboot.parse_boot_face(
            "lock_task=mLockTaskModeState=NONE\nimmersive=confirmed\n"
            "lockscreen_disabled=1\nhome_activity=com.omni.kiosk/.MainActivity\n"
            "bootanimation=/system/media/bootanimation.zip\n")
        self.assertTrue(any("lock task" in p for p in
                            silentboot.boot_face_problems(state)))

    def test_a_live_lock_screen_is_a_problem(self):
        state = silentboot.parse_boot_face(
            "lock_task=mLockTaskModeState=LOCKED\nimmersive=confirmed\n"
            "lockscreen_disabled=0\nhome_activity=com.omni.kiosk/.MainActivity\n"
            "bootanimation=/system/media/bootanimation.zip\n")
        self.assertTrue(any("lock screen" in p for p in
                            silentboot.boot_face_problems(state)))

    def test_a_foreign_home_app_is_a_problem(self):
        state = silentboot.parse_boot_face(
            "lock_task=mLockTaskModeState=LOCKED\nimmersive=confirmed\n"
            "lockscreen_disabled=1\nhome_activity=com.bliss.launcher/.Home\n"
            "bootanimation=/system/media/bootanimation.zip\n")
        self.assertTrue(any("HOME" in p for p in
                            silentboot.boot_face_problems(state)))

    def test_unreadable_output_reports_not_applied_rather_than_fine(self):
        state = silentboot.parse_boot_face("")
        self.assertTrue(silentboot.boot_face_problems(state))

    def test_the_summary_never_claims_more_than_it_read(self):
        self.assertIn("NOT",
                      silentboot.summary_line(silentboot.parse_boot_face("")))


class TheHostStillSuppressesItsOwnConsoleFlashes(unittest.TestCase):
    """tests/test_no_console_windows.py owns the behaviour; this is the
    cross-reference so the silent-boot checklist has one place to point at."""

    def test_the_default_installer_exists(self):
        from omnidroid import config
        self.assertTrue(callable(config.install_no_console_default))


class TheBootTailReportsTheFace(unittest.TestCase):
    def test_report_boot_face_is_called_from_the_shared_boot_tail(self):
        import inspect
        src = inspect.getsource(omni._ensure_booted)
        self.assertIn("report_boot_face(", src)

    def test_it_never_raises_into_the_boot(self):
        with mock.patch.object(omni, "adb",
                               side_effect=RuntimeError("adb died")):
            r = omni.report_boot_face(_acct(), "t")
        self.assertFalse(r["clean"])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_silent_boot.py -q
```
Expected: FAIL at collection — `ModuleNotFoundError: No module named 'omnidroid.silentboot'`.

- [ ] **Step 3: Write omnidroid/silentboot.py**

```python
# omnidroid/silentboot.py
"""The boot has no face until the game has one.

Nothing here is new behaviour. Every flag, policy and asset this module checks
was already written (x86 cmdline: qemu_proc.py:1834; arm cmdline + hidden GRUB
menu: engine.SILENT_CMDLINE / engine.GRUB_NO_MENU; status bar, keyguard,
wallpaper: launcher/src/com/omni/kiosk/MainActivity.java; host console
flashes: config.install_no_console_default). What did NOT exist is a way to
say WHETHER IT IS ALL STILL TRUE, per base, without a person watching a boot.

Like awake.py, farming.py and consent.py, this module is pure: it builds the
guest command sequence and parses the answer. It never touches adb.

THE SHARP EDGE, and the reason the x86 half exists at all: the production and
the interactive/builder cmdlines are built in the SAME function three lines
apart (qemu_proc.py:1829 vs :1834). An edit that lifts `console=ttyS0,115200`
out of the interactive branch puts a scrolling kernel log on a customer's
screen, and every other test in the suite would still pass.
"""
import shlex

# `sh` is farming's, and stays farming's on purpose: the adb-shell quoting
# trap it documents (adb re-parses a joined argv, so an unquoted `a; b` runs a
# fragment of itself and still reports success) is a property of the
# transport, not of any one module.
from omnidroid.farming import sh


# ---- host-side: what the kernel cmdline must and must not say --------------

# x86/Bliss, PRODUCTION boot only (qemu_proc.py:1834-1835).
X86_REQUIRED_CMDLINE = ("quiet", "loglevel=0", "console=null",
                        "vt.global_cursor_default=0", "SETUPWIZARD=0")

# The interactive/builder boot's flags (qemu_proc.py:1829). Perfectly correct
# there — a builder session with no serial log is undebuggable — and a
# customer-visible kernel log if they ever reach production.
X86_FORBIDDEN_CMDLINE = ("console=tty0", "console=ttyS0")

# arm/LineageOS (engine.SILENT_CMDLINE, engine.py:3337-3338).
ARM_REQUIRED_CMDLINE = ("quiet", "loglevel=0", "vt.global_cursor_default=0")

# ...and BEFORE the kernel loads, GRUB draws a themed menu with the LineageOS
# logo and a 10-second countdown (engine.GRUB_NO_MENU, engine.py:3340-3351).
ARM_REQUIRED_GRUB = ("set timeout=0", "set timeout_style=hidden")


def cmdline_problems(append, arm=False):
    """Reasons this kernel cmdline would show something at boot. [] = silent."""
    append = append or ""
    required = ARM_REQUIRED_CMDLINE if arm else X86_REQUIRED_CMDLINE
    problems = [f"missing {flag}" for flag in required if flag not in append]
    if not arm:
        problems += [f"{bad} leaked into a production cmdline"
                     for bad in X86_FORBIDDEN_CMDLINE if bad in append]
    return problems


def grub_problems(grub_cfg):
    """Reasons GRUB would draw a menu or a countdown. [] = straight through."""
    text = grub_cfg or ""
    return [f"missing `{need}`" for need in ARM_REQUIRED_GRUB
            if need not in text]


# ---- guest-side: what the running instance actually shows ------------------

# Read back rather than assumed, for the same reason consent.py reads its ops
# back: a policy that was SENT and a policy that APPLIED are different claims,
# and only one of them is worth printing.
_PROBE_LINES = (
    ("lock_task",
     "dumpsys activity activities 2>/dev/null | "
     "grep -m1 mLockTaskModeState | tr -d ' '"),
    ("lock_task_packages",
     "dumpsys activity activities 2>/dev/null | "
     "grep -m1 mLockTaskPackages | tr -d ' '"),
    ("immersive", "settings get secure immersive_mode_confirmations"),
    ("lockscreen_disabled", "settings get secure lockscreen.disabled"),
    ("home_activity",
     "cmd package resolve-activity -c android.intent.category.HOME "
     "--brief 0 2>/dev/null | tail -1"),
    ("bootanimation",
     "ls /system/media/bootanimation.zip /product/media/bootanimation.zip "
     "2>/dev/null | head -1"),
)


HOME_PACKAGE = "com.omni.kiosk"


def build_boot_face_probe():
    """One adb step reading back every boot-visible surface."""
    return sh("; ".join(f"echo {key}=$({cmd})" for key, cmd in _PROBE_LINES))


def parse_boot_face(text):
    """{'lock_task', ..., 'bootanimation'} from probe output.

    Never raises and never guesses: an unreadable or absent field is "", which
    reports as NOT applied rather than as silently fine."""
    text = text or ""
    out = {key: "" for key, _cmd in _PROBE_LINES}
    for line in text.splitlines():
        if "=" not in line:
            continue
        key, _sep, val = line.partition("=")
        key = key.strip()
        if key in out:
            out[key] = val.strip()
    return out


def boot_face_problems(state):
    """Reasons this instance would show a customer something it should not."""
    state = state or {}
    problems = []
    lock = (state.get("lock_task") or "").upper()
    if "LOCKED" not in lock and "PINNED" not in lock:
        problems.append(f"lock task is not engaged ({lock or 'unreadable'}) — "
                        f"the status bar can be pulled down over the game")
    if HOME_PACKAGE not in (state.get("lock_task_packages") or ""):
        problems.append("the kiosk is not in the lock task whitelist")
    if (state.get("immersive") or "").strip() != "confirmed":
        problems.append("the immersive-mode confirmation prompt is armed "
                        "('Viewing full screen / swipe down to exit')")
    if (state.get("lockscreen_disabled") or "").strip() != "1":
        problems.append("the lock screen is not disabled")
    if HOME_PACKAGE not in (state.get("home_activity") or ""):
        problems.append(f"HOME is {state.get('home_activity') or 'unreadable'}"
                        f", not {HOME_PACKAGE}")
    if not (state.get("bootanimation") or "").endswith("bootanimation.zip"):
        problems.append("no boot animation on this image — the vendor logo "
                        "or a black screen shows instead")
    return problems


def summary_line(state):
    """The one line the engine prints. Says what was READ, never what was
    sent."""
    problems = boot_face_problems(state)
    if not problems:
        return ("boot face: lock task pinned, status bar off, no lock screen, "
                "omni loading screen, kiosk is HOME")
    return "boot face: NOT clean — " + "; ".join(problems)
```

- [ ] **Step 4: Wire the report into the boot tail**

Add to `omnidroid/engine.py`, beside `assert_kiosk_ready`:

```python
def report_boot_face(acct, label):
    """Read back every boot-visible surface and say what is actually true.

    Not a fixer: everything it checks is applied elsewhere (the kernel
    cmdline at spawn, the policies by the kiosk, the settings by
    provision_settings). This is the line that turns "we checked it once, by
    eye, on one base" into something a log carries on every boot.
    """
    from omnidroid import silentboot
    try:
        r = adb(acct, *silentboot.build_boot_face_probe(), timeout=45)
        state = silentboot.parse_boot_face((r.stdout or "") + (r.stderr or ""))
    except Exception as e:  # noqa: BLE001 — a report must never fail a boot
        print(f"[{label}] boot face: could not be read ({e!r})")
        return {"clean": False, "state": None, "problems": [repr(e)]}
    problems = silentboot.boot_face_problems(state)
    print(f"[{label}] {silentboot.summary_line(state)}")
    return {"clean": not problems, "state": state, "problems": problems}
```

And in `_ensure_booted`, immediately after the `kiosk_health = assert_kiosk_ready(acct, label)` line added in Task 5:

```python
    report_boot_face(acct, label)
```

- [ ] **Step 5: Run the tests**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_silent_boot.py tests/test_no_console_windows.py -q
```
Expected: PASS.

- [ ] **Step 6: Run the checklist on BOTH bases and write it down**

x86:
```bash
omnidroid start u1
adb -s 127.0.0.1:16001 shell 'dumpsys activity activities | grep -m2 mLockTask; settings get secure immersive_mode_confirmations; settings get secure lockscreen.disabled; cmd package resolve-activity -c android.intent.category.HOME --brief 0 | tail -1; ls /system/media/bootanimation.zip'
adb -s 127.0.0.1:16001 shell am broadcast -a com.omni.kiosk.STATUS -n com.omni.kiosk/.SessionReceiver
```

ARM (over ssh, `--no-window` mandatory):
```bash
ssh 192.168.0.30 'omnidroid start u1 --no-window'
ssh 192.168.0.30 'adb -s 127.0.0.1:16001 shell "dumpsys activity activities | grep -m2 mLockTask; settings get secure immersive_mode_confirmations; settings get secure lockscreen.disabled; cmd package resolve-activity -c android.intent.category.HOME --brief 0 | tail -1; ls /product/media/bootanimation.zip"'
```

Create `docs/superpowers/runbooks/2026-08-19-C-silent-boot-checklist.md` with this exact table filled in from the output above — one row per cosmetic, one column per base, `PASS` / `FAIL` plus the literal command output that decided it, and a dated header naming the base versions (`x86` Bliss OS 16.9.7 base `v6`; `arm` LineageOS 23.2 arm64 rooted, Magisk v30.7):

```markdown
# Silent boot — verified checklist, 2026-08-19

Bases: `x86` = Bliss OS 16.9.7, base version `v6`.
       `arm` = LineageOS 23.2 arm64 (rooted pair, Magisk v30.7), on the Mac
       at 192.168.0.30 — every engine command over that ssh session used
       `--no-window`.

Automated ratchet: `tests/test_silent_boot.py`. This file is the human half —
what a person saw on a real boot on each base, on this date.

| # | Cosmetic | Where it is implemented | x86 | arm |
|---|---|---|---|---|
| 1 | Quiet kernel cmdline | `qemu_proc.py:1834` / `engine.SILENT_CMDLINE:3337` | | |
| 2 | Interactive boot keeps its serial log | `qemu_proc.py:1829` | | n/a |
| 3 | No bootloader menu or countdown | `engine.GRUB_NO_MENU:3340` | n/a | |
| 4 | Omni loading animation, no vendor logo | base `v2` bake / `_replace_bootanimation:3227` | | |
| 5 | Status bar disabled (device owner) | `MainActivity.configureLockTask` | | |
| 6 | Lock task engaged, kiosk + game whitelisted | `MainActivity.enterLockTask` | | |
| 7 | Immersive confirmation pre-confirmed | `engine.py:857` | | |
| 8 | No lock screen | `MainActivity.dismissKeyguard` + `engine.py:851` | | |
| 9 | Solid black wallpaper | `MainActivity.ensureBlackWallpaper` + `engine.py:874` | | |
| 10 | Kiosk is HOME | `engine.provision_settings` | | |
| 11 | No host console flashes | `config.install_no_console_default:179` | | n/a |
| 12 | Full disk access allowed | `consent.py:77` + the root fallback | | |

## Raw output

### x86

    <paste the exact adb output from the x86 run here, indented four spaces>

### arm

    <paste the exact adb output from the arm run here, indented four spaces>

## What the check found missing

<one line per FAIL, naming the commit that fixed it — or "nothing">
```

Anything that comes back `FAIL` is fixed in this task, with its own test in `tests/test_silent_boot.py` before the fix.

- [ ] **Step 7: Run the full suite and diff against the baseline**

```bash
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | grep '^FAILED' | sort
```
Expected: exactly the 11 baseline `FAILED` lines.

- [ ] **Step 8: Commit**

```bash
git add omnidroid/silentboot.py omnidroid/engine.py tests/test_silent_boot.py \
        docs/superpowers/runbooks/2026-08-19-C-silent-boot-checklist.md
git commit -m "feat(boot): per-base silent-boot checklist, machine-checked"
```

---

## Closing verification (run after Task 9, before calling sub-project C done)

- [ ] **Both bases, cold, end to end.** `omnidroid start u1` on x86 and `ssh 192.168.0.30 'omnidroid start u1 --no-window'` on ARM. Each log must contain, in order: `kiosk game package = com.roblox.client`, `kiosk health: device owner, lock task pinned, status bar off`, `boot face: lock task pinned, ...`, `consent: full disk access, ... error dialogs off`.
- [ ] **The screen.** `omnidroid screenshot u1` on each base: black or the Omni loading animation until Roblox appears — never a vendor wallpaper, never a status bar, never a lock screen, never a terminal.
- [ ] **It is playing, not merely running.** Per spec §8: `client_is_playing` (USER time), not "the process exists". Confirm the instance reports playing before declaring the launch path fixed.
- [ ] **Do not re-open a probabilistic failure on one lucky run** (spec §8, standing rule). Run each base's cold boot **five times**; a launch race that reappears once in five is not fixed.
- [ ] **Verify the frozen build, not the source** (spec §8, standing rule). The engine is frozen in from a sibling checkout at build time, so "the source is fixed" and "the shipped exe is fixed" are different claims. Rebuild the exe and re-run the x86 cold boot through it.

---

## Self-Review Notes

**Spec §6 coverage:**

| Spec §6 requirement | Task |
|---|---|
| "make the dev-mode 'first launchable non-system app' fallback WAIT rather than guess" | 1, 2 |
| "Bake `omni_game_package` into the image so it is set before the kiosk ever runs" | Already shipped — `bases.build_game_bake_script` (`bases.py:225-241`), `update_kiosk_arm` (`engine.py:2765-2772`), pinned by `tests/test_bake_data_game.py`. Task 2's ContentObserver is the guest-side belt to it, Task 5 verifies the result per boot. |
| "Replace the `launchedThisBoot` bool with a bounded retry" | 2 |
| "a kiosk with nothing to launch shows the loading screen, which is the correct state, not an error" | 2 (unbounded wait; `showStatus` renders nothing in production) |
| "Force-stop Roblox before every session hand-off" | 6 |
| deep-link degradation to the login screen | 3 |
| silent device-owner failures | 4, 5 |
| host-side fixed sleeps | 8 |
| "Boot cosmetics… a checked list per base, not new code — plus whatever the check finds missing" | 9 |
| "Full-disk access is already automatic… A root fallback is added for the case where adbd is not uid 0" | 7 |
| "Independent of E" | Nothing in any task touches `qemu-patches/`, `tools/build-qemu.py`, `hostwin.py` or the window flags. |
