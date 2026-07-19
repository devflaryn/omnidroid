package com.omni.bootstrap;

import android.content.Context;
import android.database.Cursor;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.webkit.CookieManager;

/**
 * The one piece of Omni that runs INSIDE Roblox.
 *
 * It exists for exactly one reason (contracts/omni-session.md §1.2): the Roblox
 * Android client has no auth parameter in its deep-link scheme, so the only way
 * to log in without replaying taps is to put a .ROBLOSECURITY cookie in its
 * WebView cookie jar — and that jar can only be written by code running as
 * Roblox's own uid. The kiosk holds the token; this reads it and installs it.
 *
 * The omni-agent injects this into the Roblox build it ships, adding ONE call at
 * the end of the app's Application.onCreate:
 *
 *   invoke-static {p0}, Lcom/omni/bootstrap/OmniBootstrap;->install(Landroid/content/Context;)V
 *
 * Design constraints, all of them learned rather than chosen:
 *
 *  - MAIN PROCESS ONLY. Roblox runs more than one process. Touching WebView from
 *    two processes that share a data directory throws
 *    ("Using WebView from more than one process at once..."), so a helper
 *    process must never take this path.
 *  - POSTED, NOT INLINE. CookieManager.getInstance() initialises WebView. If the
 *    host app calls WebView.setDataDirectorySuffix() during its own startup,
 *    initialising WebView first would make THAT call throw and take the app down
 *    with it. Posting to the main looper runs this after onCreate returns, i.e.
 *    after the app has finished its own WebView setup.
 *  - NEVER FATAL. Every failure path logs and returns. A login that does not
 *    happen is a bad session; an exception thrown out of Application.onCreate is
 *    a boot loop of someone's product.
 */
public final class OmniBootstrap {
    private static final String TAG = "OmniBootstrap";

    private static final Uri SESSION_URI =
            Uri.parse("content://com.omni.kiosk.session/session");

    /** Cookie domain/URL. The cookie is set for the whole roblox.com tree. */
    private static final String COOKIE_URL = "https://www.roblox.com";
    private static final String COOKIE_NAME = ".ROBLOSECURITY";

    private static boolean installed = false;

    private OmniBootstrap() { }

    /** Injection entry point. Safe to call more than once. */
    public static void install(final Context ctx) {
        try {
            if (installed || ctx == null) return;
            installed = true;
            if (!isMainProcess(ctx)) {
                return;
            }
            new Handler(Looper.getMainLooper()).post(new Runnable() {
                @Override public void run() {
                    try {
                        applySession(ctx);
                    } catch (Throwable t) {
                        Log.w(TAG, "session install failed: " + t);
                    }
                }
            });
        } catch (Throwable t) {
            // Deliberately swallowed: see class doc. Never take the app down.
            Log.w(TAG, "install failed: " + t);
        }
    }

    private static void applySession(Context ctx) {
        String token = readToken(ctx);
        if (token == null || token.isEmpty()) {
            Log.i(TAG, "no token published by the kiosk; leaving session alone");
            return;
        }
        CookieManager cm;
        try {
            cm = CookieManager.getInstance();
        } catch (Throwable t) {
            Log.w(TAG, "no CookieManager (WebView unavailable): " + t);
            return;
        }
        cm.setAcceptCookie(true);
        // Attributes mirror what the site itself sets. HttpOnly is accepted by
        // setCookie and keeps the cookie out of page JS.
        String value = COOKIE_NAME + "=" + token
                + "; Domain=.roblox.com; Path=/; Secure; HttpOnly";
        cm.setCookie(COOKIE_URL, value);
        cm.flush();

        // Verify rather than assume: a rejected cookie is silent otherwise, and
        // "logged in" vs "login screen" is the whole product.
        String back = cm.getCookie(COOKIE_URL);
        boolean ok = back != null && back.contains(COOKIE_NAME);
        Log.i(TAG, ok ? "session cookie installed (" + token.length() + " chars)"
                      : "session cookie was REJECTED by CookieManager");
    }

    /** The token the kiosk is publishing for us, or null. */
    private static String readToken(Context ctx) {
        Cursor c = null;
        try {
            c = ctx.getContentResolver().query(SESSION_URI, null, null, null, null);
            if (c == null) {
                // Provider absent (kiosk not installed) or it refused us.
                Log.i(TAG, "kiosk session provider unavailable");
                return null;
            }
            if (!c.moveToFirst()) return null;
            int i = c.getColumnIndex("token");
            return i < 0 ? null : c.getString(i);
        } catch (Throwable t) {
            Log.w(TAG, "session query failed: " + t);
            return null;
        } finally {
            if (c != null) {
                try {
                    c.close();
                } catch (Throwable ignore) { }
            }
        }
    }

    /**
     * True only in the app's primary process. Compares the current process name
     * against the package name, which is how Android names the main process.
     */
    private static boolean isMainProcess(Context ctx) {
        String proc = currentProcessName(ctx);
        boolean main = proc == null || proc.equals(ctx.getPackageName());
        if (!main) Log.i(TAG, "not the main process (" + proc + "); skipping");
        return main;
    }

    private static String currentProcessName(Context ctx) {
        try {
            // API 28+: no permission, no ActivityManager round-trip.
            return android.app.Application.getProcessName();
        } catch (Throwable ignore) { }
        try {
            android.app.ActivityManager am =
                    (android.app.ActivityManager) ctx.getSystemService(
                            Context.ACTIVITY_SERVICE);
            int pid = android.os.Process.myPid();
            for (android.app.ActivityManager.RunningAppProcessInfo p
                    : am.getRunningAppProcesses()) {
                if (p.pid == pid) return p.processName;
            }
        } catch (Throwable ignore) { }
        return null;
    }
}
