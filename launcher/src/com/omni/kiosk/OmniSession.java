package com.omni.kiosk;

import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.net.Uri;
import android.util.Log;

/**
 * The account session this instance is running: which Roblox user (a
 * .ROBLOSECURITY cookie) and which place to join.
 *
 * Why this class exists at all — measured against com.roblox.client 2.726.1142
 * (see contracts/omni-session.md), login and join are two SEPARATE mechanisms on
 * Android:
 *
 *   JOIN  is free. `com.roblox.client.ActivityProtocolLaunch` is exported and
 *         handles roblox:// , so a place is joined with ONE intent and zero
 *         taps. That is {@link #joinIntent}.
 *   LOGIN is not. The client parses only placeId/gameInstanceId/accessCode/
 *         linkCode/launchData/joinAttemptId/userId from a deep link — there is
 *         no authTicket parameter (that is the DESKTOP web-launch flow). Its
 *         session is the .ROBLOSECURITY cookie in its WebView cookie jar, which
 *         only code running as Roblox's own uid can write.
 *
 * So the kiosk stores the token and PUBLISHES it (via {@link SessionProvider})
 * for the in-Roblox bootstrap to pick up and install into its own cookie jar.
 * The kiosk never tries to write another app's data — that would need root, and
 * production must not ship root.
 *
 * Stored in the kiosk's own MODE_PRIVATE prefs, so no other app can read the
 * token off disk; the only way out is the provider, which checks its caller.
 */
public final class OmniSession {
    private static final String TAG = "OmniKiosk";
    private static final String PREFS = "omni_session";

    public static final String ROBLOX_PACKAGE = "com.roblox.client";

    public static final String KEY_TOKEN = "token";
    public static final String KEY_PLACE_ID = "place_id";
    public static final String KEY_GAME_INSTANCE_ID = "game_instance_id";
    public static final String KEY_ACCESS_CODE = "access_code";
    public static final String KEY_LINK_CODE = "link_code";
    public static final String KEY_LAUNCH_DATA = "launch_data";
    public static final String KEY_USER_ID = "user_id";
    public static final String KEY_UPDATED = "updated";

    private OmniSession() { }

    static SharedPreferences prefs(Context c) {
        return c.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    public static String token(Context c) {
        return prefs(c).getString(KEY_TOKEN, null);
    }

    public static long placeId(Context c) {
        return prefs(c).getLong(KEY_PLACE_ID, 0L);
    }

    public static boolean hasSession(Context c) {
        return placeId(c) > 0L;
    }

    public static void clear(Context c) {
        prefs(c).edit().clear().apply();
    }

    /**
     * The join URL, in the form ActivityProtocolLaunch parses. Optional params
     * are only appended when set: the client reads an empty value as
     * present-but-blank rather than absent.
     */
    public static Uri joinUri(Context c) {
        SharedPreferences p = prefs(c);
        long place = p.getLong(KEY_PLACE_ID, 0L);
        if (place <= 0L) return null;
        Uri.Builder b = new Uri.Builder()
                .scheme("roblox")
                .authority("experiences")
                .appendPath("start")
                .appendQueryParameter("placeId", Long.toString(place));
        appendIfSet(b, p, KEY_GAME_INSTANCE_ID, "gameInstanceId");
        appendIfSet(b, p, KEY_ACCESS_CODE, "accessCode");
        appendIfSet(b, p, KEY_LINK_CODE, "linkCode");
        appendIfSet(b, p, KEY_LAUNCH_DATA, "launchData");
        return b.build();
    }

    private static void appendIfSet(Uri.Builder b, SharedPreferences p,
                                    String key, String param) {
        String v = p.getString(key, null);
        if (v != null && !v.isEmpty()) b.appendQueryParameter(param, v);
    }

    /**
     * Intent that joins the configured place directly. Targeted at the Roblox
     * package (not a bare VIEW) so it can never resolve to a browser or a
     * disambiguation dialog — a chooser on screen would be exactly the "menu"
     * this product exists to avoid.
     */
    public static Intent joinIntent(Context c) {
        Uri uri = joinUri(c);
        if (uri == null) return null;
        Intent i = new Intent(Intent.ACTION_VIEW, uri);
        i.setPackage(ROBLOX_PACKAGE);
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return i;
    }

    public static boolean isInstalled(Context c, String pkg) {
        try {
            c.getPackageManager().getPackageInfo(pkg, 0);
            return true;
        } catch (Exception e) {
            return false;
        }
    }

    /**
     * Join the configured place. Returns null on success, else a short reason
     * the host can act on.
     */
    public static String join(Context c, String why) {
        if (!isInstalled(c, ROBLOX_PACKAGE)) return "roblox_not_installed";
        Intent i = joinIntent(c);
        if (i == null) return "no_place";
        if (c.getPackageManager().resolveActivity(i, 0) == null) {
            // The scheme handler is gone: an old or stripped Roblox build.
            return "no_deeplink_handler";
        }
        try {
            c.startActivity(i);
        } catch (Exception e) {
            Log.w(TAG, "join failed: " + e);
            return "start_failed";
        }
        Log.i(TAG, "joined place " + placeId(c) + " (" + why + ")");
        return null;
    }
}
