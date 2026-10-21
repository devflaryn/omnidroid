package com.omnidroid.spare;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * app_process's class for every app process (crate::zygote's {@code app_command}): WrapperInit.main
 * with the same arguments, {@code 0 <target sdk> <class> [args...]}, guarded. app_process runs its
 * class under AppRuntime::onStarted, which closes binder (IPCThreadState::stopProcess) as soon as
 * the class's main returns -- as an exception escaping the app's main thread returns it -- and ART
 * hands that exception to the thread's handlers only after, from DetachCurrentThread: every
 * main-thread crash's report to ActivityManager failed on the closed driver ("Error reporting
 * crash", RuntimeException: Bad file descriptor, 2026-10-19). A zygote-forked app never returns
 * there; a wrapped one does.
 */
public final class App {
    /** What {@link #guarded} runs. */
    interface Body {
        void run() throws Throwable;
    }

    /**
     * Thread.dispatchUncaughtException -- the pre-handler, then the handler, as ART calls them --
     * resolved before the app runs.
     */
    private static final Method DISPATCH = dispatcher();

    private static Method dispatcher() {
        try {
            Method m = Thread.class.getDeclaredMethod("dispatchUncaughtException", Throwable.class);
            m.setAccessible(true);
            return m;
        } catch (Throwable no) {
            return null;
        }
    }

    public static void main(final String[] args) throws Throwable {
        final Method main = Class.forName("com.android.internal.os.WrapperInit").getMethod("main", String[].class);
        guarded(new Body() {
            public void run() throws Throwable {
                main.invoke(null, (Object) args);
            }
        });
    }

    /**
     * Run {@code body}; an exception leaving it goes to the thread's handlers here, as ART would hand
     * it, while binder is open. Android's handler (KillApplicationHandler) reports it to
     * ActivityManager and ends the process; an app's own handler that returns leaves main returning
     * normally, so ART dispatches nothing a second time.
     */
    static void guarded(Body body) throws Throwable {
        Throwable crash;
        try {
            body.run();
            return;
        } catch (InvocationTargetException e) {
            // The wrapper of the body's own Method.invoke: one level.
            crash = e.getCause() != null ? e.getCause() : e;
        } catch (Throwable e) {
            crash = e;
        }
        Thread t = Thread.currentThread();
        if (DISPATCH != null) {
            try {
                DISPATCH.invoke(t, crash);
            } catch (InvocationTargetException handlerThrew) {
                // ART ignores what a handler throws.
            } catch (Throwable reflection) {
                // Not dispatched: ART's own path, as before.
                throw crash;
            }
        } else {
            try {
                t.getUncaughtExceptionHandler().uncaughtException(t, crash);
            } catch (Throwable handlerThrew) {
                // ART ignores what a handler throws.
            }
        }
    }
}
