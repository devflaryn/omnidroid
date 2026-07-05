package com.omni.kiosk;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
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

    private void launchGame(String pkg, String why) {
        Intent li = getPackageManager().getLaunchIntentForPackage(pkg);
        if (li == null) return;
        Log.i(TAG, "launching " + pkg + " (" + why + ")");
        status.setText("");
        launchedThisBoot = true;
        li.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(li);
    }
}
