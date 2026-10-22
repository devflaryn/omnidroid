package com.omnidroid.home;

import android.app.Activity;

/**
 * The kiosk device's home: a black window under the app, nothing more. Without a launcher the
 * system's home was Settings' FallbackHome, which brought up all of Settings -- a host process of
 * ~220 MiB private on every warm device and again for every pool user, for a window nobody sees
 * (the composer leaves it out). This is ~5 KiB of APK and an app process with nothing in it.
 */
public final class OmniKioskHome extends Activity {
    @Override
    public void onBackPressed() {
        // The home has nowhere to go back to.
    }
}
