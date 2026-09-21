"""Generate `src/jni/surface.rs` from the APK's dex files.

    python crates/omni-android/tools/gen_dex_surface.py Roblox-2.738.1397.apk > crates/omni-android/src/jni/surface.rs

# Why this is generated and not hand-written

`jni-surface.md` Section B lists the **128 Java class-name string literals** in `libroblox.so`,
complete: a whole-binary sweep of every other section found none. Those are exactly the names the
engine can pass to `FindClass`. What the analysis could *not* do for most of them is attribute the
member lookups to a class -- 135 of 313 direct lookups were unattributable by in-binary dataflow
alone, and Section D leaves 21 in an `!unresolved` group.

The dex has the answer directly, so this reads it: for each class-name literal that the APK's dex
declares, every field and method with its descriptor and its `static` flag.

# What the generated table is for, and what it is NOT

Every member here is declared with `Answer::Unanswered`. That is deliberate and is the whole
point:

* a `FindClass` that misses leaves a **pending exception**, and the engine's own `JNIEnvScope`
  aborts the process when one is pending that nothing cleared -- MEASURED, it is what
  `RBXCRASH: JNIException (JNI exception pending when entering JNIEnvScope)` was;
* a `GetMethodID`/`GetFieldID` that misses returns null, and jni-surface.md section 3.1's Tier 0
  members are `CHECK_NOT_NULL` aborts;
* but a **call** to a member nobody has decided the answer for must still refuse by name, because
  a fabricated value is Global Constraint 1's failure shape.

So the generated table makes the *lookups* succeed and leaves the *answers* undecided. Anything
`classes.rs` declares by hand wins: `Registry::extend_with` adds only members a class does not
already have.
"""
import sys
import zipfile
import struct
import collections
import re


def uleb(b, o):
    r = 0
    s = 0
    while True:
        x = b[o]
        o += 1
        r |= (x & 0x7f) << s
        if x < 0x80:
            return r, o
        s += 7


class Dex:
    def __init__(self, b):
        self.b = b
        (self.string_ids_size, self.string_ids_off, self.type_ids_size, self.type_ids_off,
         self.proto_ids_size, self.proto_ids_off, self.field_ids_size, self.field_ids_off,
         self.method_ids_size, self.method_ids_off, self.class_defs_size,
         self.class_defs_off) = struct.unpack_from('<12I', b, 56)
        self._str = {}

    def string(self, i):
        if i in self._str:
            return self._str[i]
        off = struct.unpack_from('<I', self.b, self.string_ids_off + 4 * i)[0]
        n, o = uleb(self.b, off)
        end = o
        while self.b[end] != 0:
            end += 1
        s = self.b[o:end].decode('utf-8', 'replace')
        self._str[i] = s
        return s

    def type(self, i):
        return self.string(struct.unpack_from('<I', self.b, self.type_ids_off + 4 * i)[0])

    def field(self, i):
        c, t, n = struct.unpack_from('<HHI', self.b, self.field_ids_off + 8 * i)
        return self.string(n), self.type(t)

    def proto(self, i):
        sh, ret, params = struct.unpack_from('<III', self.b, self.proto_ids_off + 12 * i)
        ps = []
        if params:
            cnt = struct.unpack_from('<I', self.b, params)[0]
            for k in range(cnt):
                ps.append(self.type(struct.unpack_from('<H', self.b, params + 4 + 2 * k)[0]))
        return '(' + ''.join(ps) + ')' + self.type(ret)

    def method(self, i):
        c, p, n = struct.unpack_from('<HHI', self.b, self.method_ids_off + 8 * i)
        return self.string(n), self.proto(p)

    def classes(self):
        for i in range(self.class_defs_size):
            cidx, acc, sup, iface, src, anno, data, sv = struct.unpack_from(
                '<8I', self.b, self.class_defs_off + 32 * i)
            yield self.type(cidx), data

    def members(self, data_off):
        b = self.b
        o = data_off
        sf, o = uleb(b, o)
        inf, o = uleb(b, o)
        dm, o = uleb(b, o)
        vm, o = uleb(b, o)
        fields = []
        methods = []
        idx = 0
        for _ in range(sf):
            d, o = uleb(b, o)
            a, o = uleb(b, o)
            idx += d
            n, t = self.field(idx)
            fields.append((n, t, True))
        idx = 0
        for _ in range(inf):
            d, o = uleb(b, o)
            a, o = uleb(b, o)
            idx += d
            n, t = self.field(idx)
            fields.append((n, t, False))
        idx = 0
        for _ in range(dm):
            d, o = uleb(b, o)
            a, o = uleb(b, o)
            c, o = uleb(b, o)
            idx += d
            n, p = self.method(idx)
            methods.append((n, p, bool(a & 0x8)))
        idx = 0
        for _ in range(vm):
            d, o = uleb(b, o)
            a, o = uleb(b, o)
            c, o = uleb(b, o)
            idx += d
            n, p = self.method(idx)
            methods.append((n, p, bool(a & 0x8)))
        return fields, methods


def main():
    apk = sys.argv[1]
    z = zipfile.ZipFile(apk)

    # Section B of `jni-surface-lists.txt`: every class-name literal in `.rodata`. Recovered here
    # from the binary's own bytes rather than transcribed, so the list cannot drift from the file.
    lib = None
    for name in z.namelist():
        if name.endswith('lib/arm64-v8a/libroblox.so'):
            lib = z.read(name)
    if lib is None:
        sys.exit('libroblox.so is not in the APK')
    pattern = re.compile(
        rb'(?:android|androidx|java|javax|com|org|dalvik|kotlin)(?:/[A-Za-z0-9_$]+)+')
    literals = set()
    for m in pattern.finditer(lib):
        s = m.group(0)
        # A class-name literal is NUL-terminated and does not start mid-identifier.
        if m.end() < len(lib) and lib[m.end()] != 0:
            continue
        if m.start() > 0 and lib[m.start() - 1] not in (0, 0x20):
            continue
        literals.add(s.decode())

    out = collections.OrderedDict()
    counts = collections.Counter()
    for name in sorted(z.namelist()):
        if not (name.startswith('classes') and name.endswith('.dex')):
            continue
        # `classes4.dex` is the injected payload's (D6, and jni-surface.md's scope note).
        if name == 'classes4.dex':
            continue
        d = Dex(z.read(name))
        for cname, data in d.classes():
            if not (cname.startswith('L') and cname.endswith(';')):
                continue
            jni = cname[1:-1]
            if jni not in literals or jni in out or data == 0:
                continue
            fields, methods = d.members(data)
            out[jni] = (name, fields, methods)
            counts[name] += 1

    total = sum(len(f) + len(m) for _, f, m in out.values())
    w = sys.stdout.write
    w("//! The Java surface, **generated from the APK's dex files**.\n")
    w('//!\n')
    w('//! ```text\n')
    w('//! python crates/omni-android/tools/gen_dex_surface.py %s \\\n' % apk)
    w('//!     > crates/omni-android/src/jni/surface.rs\n')
    w('//! ```\n')
    w('//!\n')
    w('//! Do not edit by hand. The generator has why this exists, what the\n')
    w('//! [`Answer::Unanswered`](super::classes::Answer::Unanswered) on every member means, and\n')
    w('//! why a hand-written declaration in [`super::classes::DECLARED`] wins over it.\n')
    w('//!\n')
    w('//! **Provenance.** %d classes and %d members, from the class-name string literals in\n'
      % (len(out), total))
    w("//! `libroblox.so`'s own `.rodata` intersected with the classes the APK's dex declares.\n")
    w('//! `classes4.dex` is excluded: D6 and `jni-surface.md`\'s scope note put the injected\n')
    w('//! payload out of scope. Per dex file: %s.\n'
      % ', '.join('`%s` %d' % (k, v) for k, v in sorted(counts.items())))
    w('\n')
    w('use super::classes::{Answer, ClassSpec, MemberSpec, Tier};\n')
    w('\n')
    w('/// One member, declared but undecided.\n')
    w("const fn u(name: &'static str, descriptor: &'static str, is_static: bool) -> MemberSpec {\n")
    w('    MemberSpec { name, descriptor, is_static, answer: Answer::Unanswered }\n')
    w('}\n\n')
    w('/// How many classes [`DEX_SURFACE`] holds. An exact figure (Global Constraint 3).\n')
    w('pub const DEX_CLASSES: usize = %d;\n\n' % len(out))
    w('/// How many members [`DEX_SURFACE`] holds, fields and methods together.\n')
    w('pub const DEX_MEMBERS: usize = %d;\n\n' % total)
    for i, (jni, (dexname, fields, methods)) in enumerate(out.items()):
        w('static M%d: &[MemberSpec] = &[\n' % i)
        for (n, p, st) in methods:
            w('    u(%s, %s, %s),\n' % (rust_str(n), rust_str(p), 'true' if st else 'false'))
        w('];\n')
        if fields:
            w('static F%d: &[MemberSpec] = &[\n' % i)
            for (n, t, st) in fields:
                w('    u(%s, %s, %s),\n' % (rust_str(n), rust_str(t), 'true' if st else 'false'))
            w('];\n')
    w('\n')
    w("/// Every class-name literal of `libroblox.so` the APK's dex declares, with its members.\n")
    w('pub static DEX_SURFACE: &[ClassSpec] = &[\n')
    for i, (jni, (dexname, fields, methods)) in enumerate(out.items()):
        w('    ClassSpec { name: %s, tier: Tier::Support, methods: M%d, fields: %s },\n'
          % (rust_str(jni), i, ('F%d' % i) if fields else '&[]'))
    w('];\n')


def rust_str(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


main()
