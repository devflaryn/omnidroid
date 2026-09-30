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
        // What WrapperInit would preload once the app is known (the zygote's preload: resources,
        // shared libraries, the graphics driver, the JCA providers), done while waiting instead.
        // `OMNI_SPARE_PRELOAD=0` in the environment leaves it to WrapperInit.
        boolean preloaded = !"0".equals(System.getenv("OMNI_SPARE_PRELOAD"));
        if (preloaded) {
            Class<?> log = Class.forName("android.util.TimingsTraceLog");
            Object trace = log.getConstructor(String.class, long.class).newInstance("SparePreload", 1L << 14);
            java.lang.reflect.Method preload = Class.forName("com.android.internal.os.ZygoteInit").getDeclaredMethod("preload", log);
            preload.setAccessible(true);
            preload.invoke(null, trace);
        }
        File go = new File(args[0]);
        while (!go.exists()) {
            Thread.sleep(10);
        }
        String[] w = new String(Files.readAllBytes(go.toPath()), "UTF-8").trim().split(" ");
        go.delete();
        int uid = Integer.parseInt(w[0]);
        android.system.Os.setgid(uid);
        android.system.Os.setuid(uid);
        Class<?> wrapper = Class.forName("com.android.internal.os.WrapperInit");
        if (preloaded) {
            // WrapperInit.main less its preload: wrapperInit(target sdk, class and args).run().
            String[] argv = new String[w.length - 2];
            System.arraycopy(w, 2, argv, 0, argv.length);
            java.lang.reflect.Method init = wrapper.getDeclaredMethod("wrapperInit", int.class, String[].class);
            init.setAccessible(true);
            ((Runnable) init.invoke(null, Integer.parseInt(w[1]), (Object) argv)).run();
            return;
        }
        // WrapperInit <pipe fd> <target sdk> <class> [args...]: no pipe.
        String[] rest = new String[w.length];
        rest[0] = "0";
        System.arraycopy(w, 1, rest, 1, w.length - 1);
        wrapper.getMethod("main", String[].class).invoke(null, (Object) rest);
    }
}
