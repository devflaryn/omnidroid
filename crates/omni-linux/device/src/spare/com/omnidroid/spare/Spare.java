package com.omnidroid.spare;

import java.io.File;
import java.nio.file.Files;

/**
 * A spare app process (crate::zygote): started ahead of need by the zygote's stand-in, it has its
 * host process, ART and binder up and then waits for the app it will become. Its one argument is a
 * file the zygote writes when ActivityManager asks for an app: {@code <uid> <target sdk> <class>
 * [args...]}. It takes the app's uid (it starts as root) and runs WrapperInit as a launched app
 * process runs it -- the pid ActivityManager was given is already this process's -- guarded as
 * {@link App} guards it. Android's classes are reached by reflection: the jar builds from java.*
 * alone.
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
        // The file is looked for every 50 ms (`OMNI_SPARE_POLL_MS`): a spare waits minutes for its
        // app, and every 10 ms was ~2% of a core for the whole of a warm device's life (a stat and a
        // wake-up through the guest's code, 100 times a second) to start the app 20 ms sooner.
        long poll = 50;
        try {
            poll = Long.parseLong(System.getenv("OMNI_SPARE_POLL_MS"));
        } catch (RuntimeException e) {
            // unset or not a number: the default
        }
        File go = new File(args[0]);
        while (!go.exists()) {
            Thread.sleep(poll);
        }
        String[] w = new String(Files.readAllBytes(go.toPath()), "UTF-8").trim().split(" ");
        go.delete();
        int uid = Integer.parseInt(w[0]);
        Class<?> os = Class.forName("android.system.Os");
        os.getMethod("setgid", int.class).invoke(null, uid);
        os.getMethod("setuid", int.class).invoke(null, uid);
        Class<?> wrapper = Class.forName("com.android.internal.os.WrapperInit");
        if (preloaded) {
            // WrapperInit.main less its preload: wrapperInit(target sdk, class and args).run().
            final String[] argv = new String[w.length - 2];
            System.arraycopy(w, 2, argv, 0, argv.length);
            final java.lang.reflect.Method init = wrapper.getDeclaredMethod("wrapperInit", int.class, String[].class);
            init.setAccessible(true);
            final int sdk = Integer.parseInt(w[1]);
            App.guarded(new App.Body() {
                public void run() throws Throwable {
                    ((Runnable) init.invoke(null, sdk, (Object) argv)).run();
                }
            });
            return;
        }
        // WrapperInit <pipe fd> <target sdk> <class> [args...]: no pipe.
        final String[] rest = new String[w.length];
        rest[0] = "0";
        System.arraycopy(w, 1, rest, 1, w.length - 1);
        final java.lang.reflect.Method main = wrapper.getMethod("main", String[].class);
        App.guarded(new App.Body() {
            public void run() throws Throwable {
                main.invoke(null, (Object) rest);
            }
        });
    }
}
