package com.omnidroid.suprobe;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import java.io.BufferedReader;
import java.io.InputStreamReader;

/**
 * Runs `su -c id -u` from an app process (an app uid, not the shell's) and logs what came back:
 * `omni-su-probe uid=<n>` when su answered, `omni-su-probe refused: ...` when it did not.
 */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("omni-su-probe", "app uid=" + android.os.Process.myUid());
        new Thread(() -> {
            try {
                Process p = new ProcessBuilder("su", "-c", "id -u").redirectErrorStream(true).start();
                BufferedReader r = new BufferedReader(new InputStreamReader(p.getInputStream()));
                String line = r.readLine();
                int code = p.waitFor();
                if (code == 0 && line != null) {
                    Log.i("omni-su-probe", "uid=" + line.trim());
                } else {
                    Log.i("omni-su-probe", "refused: exit " + code + " " + line);
                }
            } catch (Exception e) {
                Log.i("omni-su-probe", "refused: " + e);
            }
        }).start();
    }
}
