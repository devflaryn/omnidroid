package com.omni.kiosk;

import android.app.Activity;
import android.app.WallpaperManager;
import android.app.admin.DevicePolicyManager;
import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.Color;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.WindowManager;
import android.widget.TextView;

import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * Omni Kiosk: the HOME app of a single-game instance.
 *
 * - Boot -> this activity -> auto-launch the configured game.
 * - No game installed -> black screen, "no apk found".
 * - New APK installed via adb -> launch it immediately (PACKAGE_ADDED).
 * - Game exit: THIS APP NEVER DECIDES SHUTDOWN. The host watchdog owns
 *   that call (game *process* gone for a full grace period, polled via
 *   adb). When the game merely blips (dialog, ad, focus loss) its
 *   process stays alive and the host does nothing; tapping the screen
 *   re-launches/brings the game forward.
 *
 * Config: Settings.Global "omni_game_package" (set by the manager via
 * adb). If unset, dev mode: first launchable non-system package wins.
 */
public class MainActivity extends Activity {
    private static final String TAG = "OmniKiosk";
    private static final String GAME_SETTING = "omni_game_package";

    // Base-image preinstalled user apps: never auto-picked as "the game"
    // by the dev-mode fallback. The configured package always wins over
    // this list; this only guards the no-config guess.
    private static final Set<String> NON_GAME = new HashSet<>(Arrays.asList(
            "net.sourceforge.opencamera",
            "com.termux",
            "com.amaze.filemanager",
            "me.weishu.kernelsu",
            "xtr.keymapper"));

    private TextView status;
    private boolean launchedThisBoot = false;

    private final BroadcastReceiver pkgReceiver = new BroadcastReceiver() {
        @Override public void onReceive(Context c, Intent i) {
            String pkg = i.getData() != null
                    ? i.getData().getSchemeSpecificPart() : null;
            Log.i(TAG, "package event: " + i.getAction() + " " + pkg);
            if (Intent.ACTION_PACKAGE_ADDED.equals(i.getAction())
                    && pkg != null && !pkg.equals(getPackageName())) {
                // A new APK just landed (adb install in dev mode):
                // launch it right away if it is (or can be) the game.
                String game = resolveGamePackage();
                if (pkg.equals(game)) {
                    launchGame(game, "new apk installed");
                }
            }
        }
    };

    @Override protected void onCreate(Bundle b) {
        super.onCreate(b);
        keepScreenOn();
        status = new TextView(this);
        status.setBackgroundColor(Color.BLACK);
        status.setTextColor(Color.WHITE);
        status.setTextSize(24f);
        status.setGravity(Gravity.CENTER);
        status.setText("");
        setContentView(status);

        // Tap = bring the game forward (or start it if user closed it by
        // accident). Deliberate shutdown still belongs to the host.
        status.setOnClickListener(v -> {
            String game = resolveGamePackage();
            if (game != null) launchGame(game, "tap");
        });

        ensureBlackWallpaper();
        configureLockTask();

        IntentFilter f = new IntentFilter();
        f.addAction(Intent.ACTION_PACKAGE_ADDED);
        f.addAction(Intent.ACTION_PACKAGE_REPLACED);
        f.addDataScheme("package");
        registerReceiver(pkgReceiver, f);
    }

    @Override protected void onDestroy() {
        unregisterReceiver(pkgReceiver);
        super.onDestroy();
    }

    @Override public void onWindowFocusChanged(boolean has) {
        super.onWindowFocusChanged(has);
        if (has) hideSystemUi();
    }

    @Override protected void onResume() {
        super.onResume();
        hideSystemUi();
        String game = resolveGamePackage();
        if (game == null) {
            status.setText("no apk found");
            return;
        }
        if (!launchedThisBoot) {
            launchGame(game, "boot");
        } else {
            // Game left the foreground. If its process is alive this is
            // a blip and the host does nothing; if it is dead the host
            // watchdog will power us off after its grace period.
            status.setText("");
        }
    }

    /**
     * The kiosk window never lets the display sleep.
     *
     * The host applies the same guarantee device-wide over adb (see
     * awake.py), and that is the load-bearing half — this is the window-level
     * belt to its braces, and it covers a case the settings do not: the gap
     * between HOME appearing and the game taking the foreground. The kiosk is
     * HOME, so it is on screen at boot, whenever the game blips, and forever
     * on a "no apk found" instance — exactly the idle stretches with no input
     * that Android would otherwise blank.
     *
     * FLAG_KEEP_SCREEN_ON is scoped to this window, so it releases by itself
     * when the game takes over; no wakelock to leak. TURN_SCREEN_ON /
     * SHOW_WHEN_LOCKED make a boot that lands with the display already off
     * (a warm-restored instance) wake into the kiosk rather than sit dark.
     */
    private void keepScreenOn() {
        try {
            getWindow().addFlags(
                    WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
                    | WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON
                    | WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED
                    | WindowManager.LayoutParams.FLAG_DISMISS_KEYGUARD);
        } catch (Exception e) {
            Log.w(TAG, "could not set keep-screen-on flags: " + e);
        }
    }

    /** Force the system wallpaper to solid black (once), so no default
     *  Bliss wallpaper can flash between boot animation and game launch.
     *  Stored in /data, so it persists per-account. */
    private void ensureBlackWallpaper() {
        try {
            WallpaperManager wm = WallpaperManager.getInstance(this);
            Bitmap black = Bitmap.createBitmap(1, 1, Bitmap.Config.ARGB_8888);
            black.eraseColor(Color.BLACK);
            wm.setBitmap(black);
            black.recycle();
        } catch (Exception e) {
            Log.w(TAG, "could not set black wallpaper: " + e);
        }
    }

    /** If the kiosk is device owner, whitelist itself + the game for Lock
     *  Task Mode and disable the status bar. Lock Task fully blocks the
     *  status bar / Quick-Settings pull-down / nav gestures — immersive
     *  mode alone only hides the bar (it can be swiped back). */
    private void configureLockTask() {
        try {
            DevicePolicyManager dpm =
                    getSystemService(DevicePolicyManager.class);
            if (dpm == null || !dpm.isDeviceOwnerApp(getPackageName())) {
                return;
            }
            ComponentName admin =
                    new ComponentName(this, OmniDeviceAdminReceiver.class);
            // Auto-grant runtime permissions device-wide. Without this, Roblox
            // stops on Android 13+ with "Allow Roblox to send you
            // notifications?" — a dialog sitting on top of the game that
            // someone has to tap. The product promise is a launch with no menu
            // and no taps, so the device owner answers these instead of the
            // user. Only the DO can do this; it applies to permissions the app
            // requests, it does not invent new ones.
            try {
                dpm.setPermissionPolicy(
                        admin, DevicePolicyManager.PERMISSION_POLICY_AUTO_GRANT);
            } catch (Throwable t) {
                Log.w(TAG, "could not set auto-grant permission policy: " + t);
            }
            String game = resolveGamePackage();
            String[] pkgs = (game != null)
                    ? new String[]{getPackageName(), game}
                    : new String[]{getPackageName()};
            dpm.setLockTaskPackages(admin, pkgs);
            try {
                // Disable every lock-task escape surface: no status bar,
                // no notifications, no home/recents, no system info.
                dpm.setLockTaskFeatures(admin,
                        DevicePolicyManager.LOCK_TASK_FEATURE_NONE);
            } catch (Throwable ignore) { }
            try {
                dpm.setStatusBarDisabled(admin, true);
            } catch (Throwable ignore) { }
            Log.i(TAG, "device owner: lock task configured for "
                    + java.util.Arrays.toString(pkgs));
        } catch (Exception e) {
            Log.w(TAG, "configureLockTask failed: " + e);
        }
    }

    /** Enter Lock Task (pinning). Safe to call repeatedly. */
    private void enterLockTask() {
        try {
            DevicePolicyManager dpm =
                    getSystemService(DevicePolicyManager.class);
            if (dpm != null && dpm.isLockTaskPermitted(getPackageName())) {
                startLockTask();
            }
        } catch (Exception e) {
            Log.w(TAG, "startLockTask failed: " + e);
        }
    }

    private void hideSystemUi() {
        getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
    }

    /** Configured game, else (dev mode) first launchable user app. */
    private String resolveGamePackage() {
        String configured = Settings.Global.getString(
                getContentResolver(), GAME_SETTING);
        PackageManager pm = getPackageManager();
        if (configured != null && !configured.isEmpty()) {
            if (pm.getLaunchIntentForPackage(configured) != null) {
                return configured;
            }
            return null;   // configured but not installed -> "no apk found"
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

    /**
     * Start the game. When the game is Roblox AND a session is configured, this
     * JOINS THE PLACE directly (roblox://experiences/start?placeId=...) instead
     * of opening the app's home screen — that is what makes a boot land in the
     * game with no menu and no simulated taps. Everything else (a plain APK
     * under test, no session set) still gets the ordinary launcher intent.
     */
    private void launchGame(String pkg, String why) {
        // Whitelist + pin BEFORE anything starts: a game launched while it is
        // not yet a Lock Task package gets blocked by the pin, so the order
        // here is load-bearing, not cosmetic.
        whitelistForLockTask(pkg);
        enterLockTask();

        if (OmniSession.ROBLOX_PACKAGE.equals(pkg) && OmniSession.hasSession(this)) {
            String err = OmniSession.join(this, why);
            if (err == null) {
                status.setText("");
                launchedThisBoot = true;
                return;
            }
            // Fall through to the plain launcher intent: better to show Roblox's
            // own screen than a dead kiosk. The host sees the reason in logcat
            // and in the `omnidroid play` reply.
            Log.w(TAG, "deep-link join failed (" + err + "); "
                    + "falling back to the launcher intent");
        }
        Intent li = getPackageManager().getLaunchIntentForPackage(pkg);
        if (li == null) return;
        Log.i(TAG, "launching " + pkg + " (" + why + ")");
        status.setText("");
        launchedThisBoot = true;
        li.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(li);
    }

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
            }
        } catch (Exception e) {
            Log.w(TAG, "whitelist game for lock task failed: " + e);
        }
    }
}
