import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

/**
 * Loads an APK the way an app process does, with nothing specific to any app: every classes*.dex
 * goes into one PathClassLoader, every class in them is looked up (linked, not initialized), and
 * every native library in the library directory is loaded through that class loader, so each
 * JNI_OnLoad runs in the app's linker namespace.
 *
 * Usage: ApkLoad <apk> <native library dir> [library names...]
 *
 * Android-only classes are reached by reflection so this compiles against a plain JDK.
 */
public class ApkLoad {
    public static void main(String[] args) throws Throwable {
        String apk = args[0];
        String libDir = args[1];

        int dexCount = 0;
        try (ZipFile zip = new ZipFile(apk)) {
            for (Enumeration<? extends ZipEntry> e = zip.entries(); e.hasMoreElements(); ) {
                String n = e.nextElement().getName();
                if (n.matches("classes\\d*\\.dex")) dexCount++;
            }
        }
        System.out.println("dex files in the APK: " + dexCount);

        Constructor<?> pcl = Class.forName("dalvik.system.PathClassLoader")
                .getConstructor(String.class, String.class, ClassLoader.class);
        ClassLoader loader = (ClassLoader) pcl.newInstance(apk, libDir, ClassLoader.getSystemClassLoader());

        // Every class name in every dex of the APK.
        Class<?> dexFile = Class.forName("dalvik.system.DexFile");
        Object df = dexFile.getConstructor(String.class).newInstance(apk);
        @SuppressWarnings("unchecked")
        Enumeration<String> names = (Enumeration<String>) dexFile.getMethod("entries").invoke(df);
        List<String> all = new ArrayList<>();
        while (names.hasMoreElements()) all.add(names.nextElement());
        int ok = 0, failed = 0;
        List<String> failures = new ArrayList<>();
        for (String name : all) {
            try {
                Class.forName(name, false, loader);
                ok++;
            } catch (Throwable t) {
                failed++;
                if (failures.size() < 5) failures.add(name + ": " + t);
            }
        }
        System.out.println("classes: " + all.size() + ", linked " + ok + ", failed " + failed);
        for (String f : failures) System.out.println("  " + f);

        // Native libraries, through the app's class loader (its linker namespace).
        Method load = null;
        for (Method m : Runtime.class.getDeclaredMethods()) {
            Class<?>[] p = m.getParameterTypes();
            if (m.getName().equals("loadLibrary0") && p.length == 2 && p[0] == ClassLoader.class && p[1] == String.class) load = m;
        }
        Method loadWithCaller = null;
        for (Method m : Runtime.class.getDeclaredMethods()) {
            Class<?>[] p = m.getParameterTypes();
            if (m.getName().equals("loadLibrary0") && p.length == 3 && p[0] == ClassLoader.class) loadWithCaller = m;
        }
        for (int i = 2; i < args.length; i++) {
            String lib = args[i];
            try {
                if (load != null) {
                    load.setAccessible(true);
                    load.invoke(Runtime.getRuntime(), loader, lib);
                } else {
                    loadWithCaller.setAccessible(true);
                    loadWithCaller.invoke(Runtime.getRuntime(), loader, null, lib);
                }
                System.out.println("loaded lib" + lib + ".so");
            } catch (Throwable t) {
                Throwable c = t.getCause() != null ? t.getCause() : t;
                System.out.println("lib" + lib + ".so: " + c);
            }
        }
        System.out.println("done");
        // As Android's command-line tools do: the binder pool's threads would keep the VM alive.
        System.exit(0);
    }
}
