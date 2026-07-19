package com.omni.kiosk;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.util.Log;

import org.json.JSONObject;

/**
 * Host -> kiosk session channel: `omni play` / `omni session` on the host send
 * an ordered broadcast here, this stores it and (optionally) joins immediately.
 *
 * Reached from the host as:
 *   adb shell am broadcast -a com.omni.kiosk.SET_SESSION \
 *       -n com.omni.kiosk/.SessionReceiver \
 *       --es token '<.ROBLOSECURITY>' --el place_id 1818 --ez play true
 *
 * The reply goes back in the ordered broadcast's result data as JSON, so the
 * host learns what actually happened in-guest (joined / no token / Roblox
 * missing) instead of inferring it from an exit code. See
 * omni.py::kiosk_broadcast.
 *
 * EXPORTED, but guarded by android.permission.DUMP in the manifest: adb shell
 * (uid 2000) holds DUMP, and a normal third-party app cannot be granted it. So
 * the host can drive the session while an app on the device cannot hijack it.
 */
public class SessionReceiver extends BroadcastReceiver {
    private static final String TAG = "OmniKiosk";

    public static final String ACTION_SET_SESSION = "com.omni.kiosk.SET_SESSION";
    public static final String ACTION_CLEAR_SESSION = "com.omni.kiosk.CLEAR_SESSION";

    @Override public void onReceive(Context c, Intent i) {
        String action = i.getAction();
        JSONObject out = new JSONObject();
        try {
            if (ACTION_CLEAR_SESSION.equals(action)) {
                OmniSession.clear(c);
                SessionProvider.notifyChanged(c);
                out.put("ok", true).put("cleared", true);
                Log.i(TAG, "session cleared");
            } else if (ACTION_SET_SESSION.equals(action)) {
                apply(c, i, out);
            } else {
                out.put("ok", false).put("error", "unknown_action");
            }
        } catch (Exception e) {
            Log.w(TAG, "session broadcast failed: " + e);
            try {
                out.put("ok", false).put("error", "exception")
                   .put("detail", String.valueOf(e));
            } catch (Exception ignore) { }
        }
        // Ordered broadcast reply -> `am broadcast` prints it as data="...".
        setResult(android.app.Activity.RESULT_OK, out.toString(), null);
    }

    private void apply(Context c, Intent i, JSONObject out) throws Exception {
        long place = i.getLongExtra(OmniSession.KEY_PLACE_ID, 0L);
        if (place <= 0L) {
            out.put("ok", false).put("error", "no_place");
            return;
        }
        SharedPreferences.Editor e = OmniSession.prefs(c).edit();
        e.putLong(OmniSession.KEY_PLACE_ID, place);
        putIfPresent(e, i, OmniSession.KEY_TOKEN);
        putIfPresent(e, i, OmniSession.KEY_GAME_INSTANCE_ID);
        putIfPresent(e, i, OmniSession.KEY_ACCESS_CODE);
        putIfPresent(e, i, OmniSession.KEY_LINK_CODE);
        putIfPresent(e, i, OmniSession.KEY_LAUNCH_DATA);
        if (i.hasExtra(OmniSession.KEY_USER_ID)) {
            e.putLong(OmniSession.KEY_USER_ID,
                      i.getLongExtra(OmniSession.KEY_USER_ID, 0L));
        }
        e.putLong(OmniSession.KEY_UPDATED, System.currentTimeMillis());
        // commit(), not apply(): the join below (and the bootstrap's provider
        // read that follows it) must not race an async disk write.
        e.commit();

        boolean hasToken = OmniSession.token(c) != null;
        // Tell the in-Roblox bootstrap the token changed.
        //
        // A cookie swap only takes effect on a COLD Roblox process: the client
        // reads its cookie jar during startup and caches the authenticated user,
        // so a new .ROBLOSECURITY dropped on a live process would still join as
        // the previous account. Killing it is deliberately NOT done here — the
        // kiosk is a normal app, so killBackgroundProcesses() would no-op on a
        // FOREGROUND Roblox, i.e. fail exactly when switching accounts. The host
        // owns that step (`am force-stop` as shell, which really does stop it);
        // see omni.py::deliver_session.
        SessionProvider.notifyChanged(c);

        out.put("ok", true).put("place_id", place).put("has_token", hasToken);
        if (!hasToken) out.put("warning", "no_token_stored");

        if (i.getBooleanExtra("play", false)) {
            String err = OmniSession.join(c, "host session");
            out.put("launched", err == null);
            if (err != null) out.put("error", err).put("ok", false);
        } else {
            out.put("launched", false);
        }
    }

    private static void putIfPresent(SharedPreferences.Editor e, Intent i,
                                     String key) {
        if (i.hasExtra(key)) {
            String v = i.getStringExtra(key);
            if (v == null || v.isEmpty()) {
                e.remove(key);
            } else {
                e.putString(key, v);
            }
        }
    }

}
