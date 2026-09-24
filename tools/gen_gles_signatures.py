"""Generate `crates/omni-android/src/gles/signatures.rs` from the Khronos registries.

    python3 tools/gen_gles_signatures.py <gl.xml> <egl.xml> > crates/omni-android/src/gles/signatures.rs

The GLES forwarder calls a host driver's function with the guest's arguments. Under identity
mapping the *values* need no translation, but the *calling convention* does: the guest passes
integer/pointer arguments in x0-x7 then the stack and floating-point ones in v0-v7 (AAPCS64), and
the host expects SysV (Linux, macOS x86-64) or the Microsoft x64 convention (Windows), where an
argument's register depends on its position among all arguments. So every command needs its exact
parameter classes, in order, and its return class. That list is the registry's, not ours: this
script reads it and emits it, and nothing in the table is typed by hand.

Two views of each parameter list are emitted, because two different conventions consume them:

* `params` -- the **guest** view, AAPCS64's own classes: `I` an integer or pointer (a general
  register, then an 8-byte stack slot), `F` a `float` by value (an `S` register, then a stack
  slot), `D` a `double` by value. This is what decides which guest register an argument is read
  out of.
* `abi` -- the **host** view, the exact C width of each parameter, which is what the typed
  `extern "C"` function pointer the host call goes through is declared with: `P` 64 bits
  (pointers, `GLintptr`, `GLint64`, `GLsync`, every EGL handle and `EGLAttrib`), `W` 32 bits
  (`GLenum`, `GLint`, `GLuint`, `GLsizei`, `GLbitfield`, `EGLint`, `EGLBoolean`, ...), `B` 8 bits
  unsigned (`GLboolean`), `F` `float`, `D` `double`. Exact widths rather than "everything is 64
  bits" because a host compiler may rely on the caller having extended a narrow argument (clang
  does for `bool`/`char` on SysV), and because an ABI that packs stack arguments by their natural
  size (Apple arm64) would read a 64-bit slot as two arguments.

The return class is likewise given twice: `ret` (`V` void, `I` integer/pointer, `F` float) and
`abi_ret` (`V`, `P` 64 bits, `W` unsigned 32, `S` signed 32 -- `GLint`, `GLsizei`, `EGLint`, so
that a `-1` from `glGetUniformLocation` reaches the guest sign-extended --, `B` unsigned 8, `F`).

An unknown C type is an error, not a guess: the script stops naming the command and the type.

Which commands: every command any `gles2` feature (`GL_ES_VERSION_2_0` .. `3_2`) or any extension
whose `supported` list names `gles2` requires, and every EGL command, each with the feature or
extension that first requires it. Which of them a host really has is the host's answer at run
time -- the table only says how to call one.

The script also emits one typed caller per distinct `(abi_ret, abi)` shape: an `unsafe fn` that
transmutes a host function address into exactly that `extern "C"` function-pointer type and calls
it, so the Rust compiler -- not this project -- applies the host's calling convention.
"""

import hashlib
import sys
import xml.etree.ElementTree as ET

FLOAT_TYPES = {"GLfloat", "GLclampf"}
DOUBLE_TYPES = {"GLdouble", "GLclampd"}

# Exact widths of every by-value C type the gles2 and EGL registries use. `S` is a signed 32-bit
# value (it matters only for returns: the guest must see it sign-extended).
WIDTHS = {
    # 64-bit: handles, pointer-sized integers, 64-bit integers.
    **{t: "P" for t in (
        "GLintptr", "GLsizeiptr", "GLint64", "GLint64EXT", "GLuint64", "GLuint64EXT", "GLsync",
        "GLeglImageOES", "GLeglClientBufferEXT", "GLDEBUGPROC", "GLDEBUGPROCKHR",
        "GLDEBUGPROCAMD", "GLVULKANPROCNV", "GLvdpauSurfaceNV",
        "EGLAttrib", "EGLAttribKHR", "EGLDisplay", "EGLConfig", "EGLContext", "EGLSurface",
        "EGLImage", "EGLImageKHR", "EGLSync", "EGLSyncKHR", "EGLSyncNV", "EGLClientBuffer",
        "EGLNativeDisplayType", "EGLNativeWindowType", "EGLNativePixmapType", "EGLStreamKHR",
        "EGLDeviceEXT", "EGLOutputLayerEXT", "EGLOutputPortEXT", "EGLLabelKHR", "EGLObjectKHR",
        "EGLTime", "EGLTimeKHR", "EGLTimeNV", "EGLuint64KHR", "EGLuint64NV", "EGLnsecsANDROID",
        "EGLDEBUGPROCKHR", "EGLGetBlobFuncANDROID", "EGLSetBlobFuncANDROID",
        "__eglMustCastToProperFunctionPointerType", "EGLFrameTokenANGLE",
    )},
    # 32-bit unsigned (or signed where only an argument; the width is what matters there).
    **{t: "W" for t in (
        "GLenum", "GLuint", "GLbitfield", "EGLenum", "EGLBoolean",
    )},
    **{t: "S" for t in (
        "GLint", "GLsizei", "GLfixed", "GLclampx", "EGLint", "EGLNativeFileDescriptorKHR",
    )},
    "GLboolean": "B",
}


def type_text(elem):
    text = "".join(elem.itertext())
    return text.rsplit(elem.find("name").text, 1)[0]


def classes(elem, command):
    """(guest class, host abi class) of one <param> or <proto>."""
    text = type_text(elem)
    ptype = elem.find("ptype")
    name = ptype.text if ptype is not None else None
    if "*" in text:
        return "I", "P"
    if name in FLOAT_TYPES:
        return "F", "F"
    if name in DOUBLE_TYPES:
        return "D", "D"
    if name is None:
        stripped = text.replace("const", "").strip()
        if stripped == "void":
            return "V", "V"
        raise SystemExit(f"{command}: a bare C type `{stripped}` this script has no width for")
    if name not in WIDTHS:
        raise SystemExit(f"{command}: the type `{name}` has no width in WIDTHS")
    return "I", WIDTHS[name]


def commands(root, wanted):
    """The classes of every command in `wanted` (a registry's other commands are not read, so a
    desktop-only type with no width here cannot stop the script)."""
    out = {}
    for block in root.iter("commands"):
        for cmd in block.findall("command"):
            proto = cmd.find("proto")
            name = proto.find("name").text
            if name not in wanted:
                continue
            ret, abi_ret = classes(proto, name)
            params, abi = "", ""
            for p in cmd.findall("param"):
                g, h = classes(p, name)
                params += g
                # A parameter's width is what the host reads; signedness only matters for returns.
                abi += "W" if h == "S" else h
            out[name] = (ret, abi_ret, params, abi)
    return out


def origins(root, api):
    names = {}
    for feature in root.iter("feature"):
        if feature.get("api") != api:
            continue
        for req in feature.iter("require"):
            if req.get("api") not in (None, api):
                continue
            for c in req.iter("command"):
                names.setdefault(c.get("name"), feature.get("name"))
    for ext in root.iter("extension"):
        supported = (ext.get("supported") or "").split("|")
        if api not in supported:
            continue
        for req in ext.iter("require"):
            if req.get("api") not in (None, api):
                continue
            for c in req.iter("command"):
                names.setdefault(c.get("name"), ext.get("name"))
    return names


RUST_ARG = {"P": "u64", "W": "u32", "B": "u8", "F": "f32", "D": "f64"}
RUST_RET = {"P": "u64", "W": "u32", "S": "i32", "B": "u8", "F": "f32"}


def lane(cls, i):
    if cls == "P":
        return f"a[{i}]"
    if cls == "W":
        return f"a[{i}] as u32"
    if cls == "B":
        return f"a[{i}] as u8"
    if cls == "F":
        return f"f32::from_bits(a[{i}] as u32)"
    if cls == "D":
        return f"f64::from_bits(a[{i}])"
    raise AssertionError(cls)


def widen(abi_ret):
    return {
        "P": "r",
        "W": "u64::from(r)",
        "S": "i64::from(r) as u64",
        "B": "u64::from(r)",
        "F": "u64::from(r.to_bits())",
    }[abi_ret]


def caller_name(abi_ret, abi):
    return f"call_{abi_ret.lower()}_{abi.lower() or 'none'}"


def main():
    gl_path, egl_path = sys.argv[1], sys.argv[2]
    gl_bytes, egl_bytes = open(gl_path, "rb").read(), open(egl_path, "rb").read()
    gl, egl = ET.fromstring(gl_bytes), ET.fromstring(egl_bytes)
    gles, egls = origins(gl, "gles2"), origins(egl, "egl")
    all_egl = {c.find("proto").find("name").text for b in egl.iter("commands") for c in b.findall("command")}
    gl_cmds, egl_cmds = commands(gl, gles), commands(egl, all_egl)
    rows = []
    for name in sorted(gles):
        rows.append((name, *gl_cmds[name], gles[name]))
    for name in sorted(egl_cmds):
        if name not in egls:
            raise SystemExit(f"{name}: an EGL command no feature or extension requires")
        rows.append((name, *egl_cmds[name], egls[name]))
    # Distinct host shapes, in first-seen order of a sorted walk so the file is stable.
    shapes = sorted({(r[2], r[4]) for r in rows})
    shape_index = {s: i for i, s in enumerate(shapes)}

    w = sys.stdout.write
    w("// @generated by tools/gen_gles_signatures.py -- do not edit by hand.\n")
    w(f"// gl.xml  sha256 {hashlib.sha256(gl_bytes).hexdigest()} (KhronosGroup/OpenGL-Registry, Apache-2.0)\n")
    w(f"// egl.xml sha256 {hashlib.sha256(egl_bytes).hexdigest()} (KhronosGroup/EGL-Registry, Apache-2.0)\n")
    w("//! Every GLES (`gles2` API: ES 2.0-3.2 and its extensions) and EGL command's calling shape,\n")
    w("//! from the Khronos registries, and one typed host caller per distinct shape. See\n")
    w("//! `tools/gen_gles_signatures.py` for the classes and for why there are two views of each.\n\n")
    w("/// One command: its name, its classes in both views, where the registry defines it, and the\n")
    w("/// index of its host caller in [`SHAPES`].\n")
    w("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n")
    w("pub struct Signature {\n    /// The command's name.\n    pub name: &'static str,\n")
    w("    /// Guest return class: `V`, `I` or `F`.\n    pub ret: u8,\n")
    w("    /// Guest parameter classes (AAPCS64): one of `I`, `F`, `D` per parameter, in order.\n    pub params: &'static str,\n")
    w("    /// Host return width: `V`, `P`, `W`, `S`, `B` or `F`.\n    pub abi_ret: u8,\n")
    w("    /// Host parameter widths: one of `P`, `W`, `B`, `F`, `D` per parameter, in order.\n    pub abi: &'static str,\n")
    w("    /// The feature or extension that first requires it.\n    pub origin: &'static str,\n")
    w("    /// Its caller's index in [`SHAPES`].\n    pub shape: u16,\n}\n\n")
    w("/// One distinct host calling shape and the typed caller for it.\n")
    w("#[derive(Clone, Copy)]\n")
    w("pub struct Shape {\n    /// Host return width, as [`Signature::abi_ret`].\n    pub abi_ret: u8,\n")
    w("    /// Host parameter widths, as [`Signature::abi`].\n    pub abi: &'static str,\n")
    w("    /// The caller: `call(f, lanes)` calls the host function at `f` with `lanes.len() ==\n")
    w("    /// abi.len()` argument bit patterns and returns its result widened to 64 bits.\n")
    w("    ///\n    /// # Safety\n    ///\n")
    w("    /// `f` must be the address of a host function whose C prototype has exactly this shape.\n")
    w("    pub call: unsafe fn(usize, &[u64]) -> u64,\n}\n\n")
    w("impl core::fmt::Debug for Shape {\n    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {\n")
    w("        write!(f, \"Shape({}, {:?})\", self.abi_ret as char, self.abi)\n    }\n}\n\n")
    w(f"/// {len(rows)} commands, sorted by name within GLES then EGL.\n")
    w("pub static SIGNATURES: &[Signature] = &[\n")
    for name, ret, abi_ret, params, abi, origin in rows:
        w(f"    Signature {{ name: \"{name}\", ret: b'{ret}', params: \"{params}\", abi_ret: b'{abi_ret}', "
          f"abi: \"{abi}\", origin: \"{origin}\", shape: {shape_index[(abi_ret, abi)]} }},\n")
    w("];\n\n")
    w(f"/// {len(shapes)} distinct host shapes, sorted.\n")
    w("pub static SHAPES: &[Shape] = &[\n")
    for abi_ret, abi in shapes:
        w(f"    Shape {{ abi_ret: b'{abi_ret}', abi: \"{abi}\", call: {caller_name(abi_ret, abi)} }},\n")
    w("];\n")
    for abi_ret, abi in shapes:
        args = ", ".join(RUST_ARG[c] for c in abi)
        ret = "" if abi_ret == "V" else f" -> {RUST_RET[abi_ret]}"
        call = ", ".join(lane(c, i) for i, c in enumerate(abi))
        w("\n")
        w(f"/// `{abi_ret}({abi})`.\n///\n/// # Safety\n///\n/// See [`Shape::call`].\n")
        w(f"unsafe fn {caller_name(abi_ret, abi)}(f: usize, a: &[u64]) -> u64 {{\n")
        w(f"    debug_assert_eq!(a.len(), {len(abi)});\n")
        if abi:
            w(f"    let _ = &a[..{len(abi)}];\n")
        w("    // SAFETY: the caller guarantees `f` is a host function of exactly this prototype, and a\n")
        w("    // function pointer is address-sized on every host this crate builds for.\n")
        w(f"    let f: extern \"C\" fn({args}){ret} = unsafe {{ core::mem::transmute::<usize, extern \"C\" fn({args}){ret}>(f) }};\n")
        if abi_ret == "V":
            w(f"    f({call});\n    0\n")
        else:
            w(f"    let r = f({call});\n    {widen(abi_ret)}\n")
        w("}\n")


if __name__ == "__main__":
    main()
