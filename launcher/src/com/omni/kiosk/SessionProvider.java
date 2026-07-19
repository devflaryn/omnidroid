package com.omni.kiosk;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.util.Log;

/**
 * Kiosk -> Roblox session channel. The ONLY way the token leaves the kiosk.
 *
 * The in-Roblox bootstrap (a small component the omni-agent injects into the
 * Roblox build it ships — see contracts/omni-session.md) queries this on startup
 * and installs the returned .ROBLOSECURITY into its own WebView cookie jar. That
 * indirection exists because nothing outside Roblox's uid can write that jar:
 * the kiosk cannot, and production must not ship root to do it.
 *
 * Access control: this is exported (it must be — the caller is another app), so
 * every query checks {@link #getCallingPackage()} and answers only Roblox. A
 * different app asking gets null, not the token. Package names are unique and
 * system-enforced, so on a device whose only two apps are the kiosk and Roblox
 * this is a real boundary rather than a decorative one.
 */
public class SessionProvider extends ContentProvider {
    private static final String TAG = "OmniKiosk";

    public static final String AUTHORITY = "com.omni.kiosk.session";
    public static final Uri URI = Uri.parse("content://" + AUTHORITY + "/session");

    /** Columns the bootstrap reads. Order is part of the contract. */
    private static final String[] COLUMNS = {
            "token", "place_id", "user_id", "updated",
    };

    @Override public boolean onCreate() {
        return true;
    }

    @Override public Cursor query(Uri uri, String[] projection, String selection,
                                  String[] selectionArgs, String sortOrder) {
        Context c = getContext();
        if (c == null) return null;
        String caller = getCallingPackage();
        if (!OmniSession.ROBLOX_PACKAGE.equals(caller)) {
            Log.w(TAG, "session query REFUSED for caller: " + caller);
            return null;
        }
        MatrixCursor cur = new MatrixCursor(COLUMNS, 1);
        cur.addRow(new Object[]{
                OmniSession.token(c),
                OmniSession.placeId(c),
                OmniSession.prefs(c).getLong(OmniSession.KEY_USER_ID, 0L),
                OmniSession.prefs(c).getLong(OmniSession.KEY_UPDATED, 0L),
        });
        cur.setNotificationUri(c.getContentResolver(), URI);
        return cur;
    }

    /** Wake any bootstrap observing the session (token/place changed). */
    static void notifyChanged(Context c) {
        try {
            c.getContentResolver().notifyChange(URI, null);
        } catch (Exception e) {
            Log.w(TAG, "notifyChange failed: " + e);
        }
    }

    @Override public String getType(Uri uri) {
        return "vnd.android.cursor.item/vnd.com.omni.kiosk.session";
    }

    // Read-only: the host owns the session, the app only consumes it.
    @Override public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("session is read-only");
    }

    @Override public int delete(Uri uri, String s, String[] a) {
        throw new UnsupportedOperationException("session is read-only");
    }

    @Override public int update(Uri uri, ContentValues v, String s, String[] a) {
        throw new UnsupportedOperationException("session is read-only");
    }
}
