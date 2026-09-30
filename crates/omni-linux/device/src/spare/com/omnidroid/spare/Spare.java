package com.omnidroid.spare;

import java.io.File;
import java.nio.file.Files;

/**
 * A spare app process (crate::zygote): started ahead of need by the zygote's stand-in, it has its
 * host process, ART and binder up and then waits for the app it will become. Its one argument is a
 * file the zygote writes when ActivityManager asks for an app: {@code <uid> <target sdk> <class>
 * [args...]}. It takes the app's uid (it starts as root) and runs WrapperInit as a launched app
 * process runs it -- the pid ActivityManager was given is already this process's.
 */
public final class Spare {
    public static void main(String[] args) throws Throwable {
        File go = new File(args[0]);
        while (!go.exists()) {
            Thread.sleep(10);
        }
        String[] w = new String(Files.readAllBytes(go.toPath()), "UTF-8").trim().split(" ");
        go.delete();
        int uid = Integer.parseInt(w[0]);
        android.system.Os.setgid(uid);
        android.system.Os.setuid(uid);
        // WrapperInit <pipe fd> <target sdk> <class> [args...]: no pipe.
        String[] rest = new String[w.length];
        rest[0] = "0";
        System.arraycopy(w, 1, rest, 1, w.length - 1);
        Class.forName("com.android.internal.os.WrapperInit").getMethod("main", String[].class).invoke(null, (Object) rest);
    }
}
