#!/usr/bin/env python3
"""Generate omnidroid's Vulkan forwarding (D3a) from the Vulkan registry.

Design: docs/superpowers/specs/2026-09-27-d3a-guest-vulkan-design.md
Plan:   docs/superpowers/plans/2026-09-27-d3a-guest-vulkan.md ("Interfaces fixed here")

Reads tools/vk/vk.xml (the registry of the NDK r28c headers, 1.3.275) and tools/vk/special.txt
(the commands forwarded by hand on both sides) and writes, deterministically:

  crates/omni-linux/src/gpu/generated.rs           host: ids, COMMANDS, EXTENSIONS, dispatch()
  crates/omni-linux/device/src/vk/generated.h      guest: ids, the entry table's type, the
                                                   special entry points' prototypes
  crates/omni-linux/device/src/vk/generated.c      guest: every generic entry point, and the
                                                   name -> function table (sorted, bsearch-able)

A guest pointer is a host pointer and Vulkan's structs have one layout on both sides, so a command
is forwarded as its arguments widened to 64 bits; the host calls the host driver's function with
the command's exact signature. What cannot be forwarded that way is either special (hand-written,
special.txt) or excluded (its extension is not offered to the guest at all). The rules for both
are data, just below. The generator refuses a command it cannot classify.

A report (what was excluded and why, and the pNext structures that carry dispatchable handles)
goes to stderr.

Usage: python tools/gen_vk_forward.py [--xml PATH] [--special PATH] [--out-root DIR]
Python 3 standard library only.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
import xml.etree.ElementTree as ET

# ------------------------------------------------------------------------------------------------
# The rules, as data.
# ------------------------------------------------------------------------------------------------

#: The registry version this generator is written against (the NDK r28c headers' version).
VK_XML_VERSION = "1.3.275"
#: The API variant whose commands, types and members are read (vk.xml also describes Vulkan SC).
API = "vulkan"
#: Core versions whose commands are all forwarded. Any VK_VERSION_* named here is true in a
#: `depends` expression; any other version (and every VKSC_*) is false.
CORE_FEATURES = ("VK_VERSION_1_0", "VK_VERSION_1_1", "VK_VERSION_1_2", "VK_VERSION_1_3")

#: Extension `platform` attributes that are forwarded. Every other platform (win32, xlib, metal,
#: fuchsia, provisional, ...) is excluded: its types come from headers the guest does not have or
#: its handles mean nothing on the other side.
ALLOWED_PLATFORMS = ("android",)

#: Platform types (native header types, and the opaque basetypes such as AHardwareBuffer) an
#: extension of a given platform may use. The Android ones are implemented by the host (special).
PLATFORM_TYPES_ALLOWED = {"android": ("AHardwareBuffer", "ANativeWindow")}

#: Extensions excluded by name, with why. An extension whose `depends` cannot hold without these
#: (VK_KHR_surface, VK_KHR_swapchain, VK_KHR_display: every surface/present/display extension) is
#: excluded as well, transitively, as is an extension whose `depends` needs any excluded one.
EXCLUDED = {
    "VK_KHR_surface": "surfaces are the Android loader's (it implements them itself)",
    "VK_KHR_swapchain": "swapchains are the Android loader's (on VK_ANDROID_native_buffer)",
    "VK_KHR_display": "displays are the Android loader's, not the host's to give",
    "VK_EXT_debug_report": "its callback is a guest function pointer the host cannot call",
    "VK_EXT_debug_utils": "its messenger callback is a guest function pointer the host cannot call",
    "VK_EXT_device_memory_report": "its callback is a guest function pointer the host cannot call",
    "VK_KHR_external_memory_fd": "external memory by fd: a host fd is not a guest fd (nor the reverse)",
    "VK_EXT_external_memory_dma_buf": "external memory by dma-buf fd: a host fd is not a guest fd",
    "VK_EXT_external_memory_host": "host-pointer import; zero-copy gralloc is a later, measured step (design)",
    "VK_NV_external_memory_rdma": "exports an external (RDMA) memory handle, meaningless across the boundary",
}

#: Structures whose function-pointer members do not exclude a command: the guest driver always
#: passes pAllocator = NULL (the design), so the host never sees a guest function pointer in one.
FUNCPOINTER_EXEMPT = {"VkAllocationCallbacks": "pAllocator is always NULL from the guest driver"}

#: The dispatchable handle types (VK_DEFINE_HANDLE); read from vk.xml and checked against this.
DISPATCHABLE = ("VkCommandBuffer", "VkDevice", "VkInstance", "VkPhysicalDevice", "VkQueue")

#: Scalar C types -> how a value travels. "addr": a pointer-sized value (pointer or
#: non-dispatchable handle), "int": an integer/enum of the named Rust type, "f32"/"f64": bits.
BASIC_TYPES = {
    "uint64_t": ("int", "u64"), "int64_t": ("int", "i64"), "size_t": ("int", "u64"),
    "uint32_t": ("int", "u32"), "int32_t": ("int", "i32"),
    "uint16_t": ("int", "u16"), "int16_t": ("int", "i16"),
    "uint8_t": ("int", "u8"), "int8_t": ("int", "i8"),
    "int": ("int", "i32"),
    "float": ("f32", "f32"), "double": ("f64", "f64"),
}
BASETYPES = {
    "VkBool32": ("int", "u32"), "VkSampleMask": ("int", "u32"), "VkFlags": ("int", "u32"),
    "VkFlags64": ("int", "u64"), "VkDeviceSize": ("int", "u64"), "VkDeviceAddress": ("int", "u64"),
    "VkRemoteAddressNV": ("addr", "u64"),
}
#: Return types a generic command may have: C type -> kind.
RETURNS = {
    "void": "void", "VkResult": "result",
    "VkBool32": "u32", "uint32_t": "u32",
    "uint64_t": "u64", "VkDeviceAddress": "u64", "VkDeviceSize": "u64",
}

HOST_OUT = os.path.join("crates", "omni-linux", "src", "gpu", "generated.rs")
GUEST_H_OUT = os.path.join("crates", "omni-linux", "device", "src", "vk", "generated.h")
GUEST_C_OUT = os.path.join("crates", "omni-linux", "device", "src", "vk", "generated.c")
BANNER = f"generated by tools/gen_vk_forward.py from vk.xml {VK_XML_VERSION} -- do not edit"


class GenError(Exception):
    """A command or rule the generator cannot classify; a human decides (special.txt or EXCLUDED)."""


# ------------------------------------------------------------------------------------------------
# The registry.
# ------------------------------------------------------------------------------------------------


def api_ok(el) -> bool:
    api = el.get("api")
    return api is None or API in api.split(",")


def decl_text(el) -> str:
    """The C declaration an element (<param>, <member>, <proto>) spells, without comments."""
    parts = [el.text or ""]
    for child in el:
        if child.tag != "comment":
            parts.append(child.text or "")
        parts.append(child.tail or "")
    return " ".join("".join(parts).split()).replace(" [", "[").replace("[ ", "[").replace(" ]", "]")


class Decl:
    """A parameter or struct member."""

    def __init__(self, el):
        self.type = el.find("type").text
        self.name = el.find("name").text
        self.decl = decl_text(el)
        tail = self.decl.rsplit(self.name, 1)[1] if self.name in self.decl else ""
        self.array = "[" in tail
        self.pointer = "*" in self.decl or self.array


class Command:
    def __init__(self, el):
        proto = el.find("proto")
        self.name = proto.find("name").text
        self.ret = proto.find("type").text
        self.ret_decl = decl_text(proto).rsplit(self.name, 1)[0].strip()
        self.params = [Decl(p) for p in el.findall("param") if api_ok(p)]


class Registry:
    def __init__(self, path: str):
        root = ET.parse(path).getroot()
        self.types: dict[str, ET.Element] = {}
        for t in root.find("types").findall("type"):
            if not api_ok(t):
                continue
            name = t.get("name") or (t.find("name").text if t.find("name") is not None else None)
            if name:
                self.types[name] = t
        self.bitwidth64 = {e.get("name") for e in root.findall("enums") if e.get("bitwidth") == "64"}
        self.commands: dict[str, Command] = {}
        self.cmd_alias: dict[str, str] = {}
        for c in root.find("commands").findall("command"):
            if not api_ok(c):
                continue
            if c.get("alias"):
                self.cmd_alias[c.get("name")] = c.get("alias")
            else:
                cmd = Command(c)
                self.commands[cmd.name] = cmd
        self.features = {f.get("name"): f for f in root.findall("feature") if api_ok(f)}
        self.extensions = {e.get("name"): e for e in root.find("extensions").findall("extension")}
        header = [t for t in self.types.values() if t.get("category") == "define" and t.find("name") is not None
                  and t.find("name").text == "VK_HEADER_VERSION"]
        version = re.search(r"(\d+)\s*$", decl_text(header[0])).group(1) if header else "?"
        if not VK_XML_VERSION.endswith("." + version):
            raise GenError(f"vk.xml is header version {version}, this generator is for {VK_XML_VERSION}")
        found = sorted(n for n, t in self.types.items() if t.get("category") == "handle" and not t.get("alias")
                       and t.find("type") is not None and t.find("type").text == "VK_DEFINE_HANDLE")
        if tuple(found) != DISPATCHABLE:
            raise GenError(f"dispatchable handles in vk.xml are {found}, expected {list(DISPATCHABLE)}")

    # --- types --------------------------------------------------------------------------------

    def resolve(self, name: str) -> str:
        seen = set()
        while name in self.types and self.types[name].get("alias") and name not in seen:
            seen.add(name)
            name = self.types[name].get("alias")
        return name

    def category(self, name: str):
        t = self.types.get(self.resolve(name))
        return None if t is None else t.get("category")

    def is_dispatchable(self, name: str) -> bool:
        return self.resolve(name) in DISPATCHABLE

    def members(self, name: str) -> list[Decl]:
        t = self.types.get(self.resolve(name))
        return [Decl(m) for m in t.findall("member") if api_ok(m)] if t is not None else []

    def is_platform_type(self, name: str) -> bool:
        t = self.types.get(self.resolve(name))
        if t is None:
            return False
        req = t.get("requires") or ""
        if req.endswith(".h") and not req.startswith("vk_video/"):
            return True
        # Opaque basetypes: `struct ANativeWindow;`, the Metal ids, IOSurfaceRef.
        return t.get("category") == "basetype" and t.find("type") is None

    def reach(self, roots):
        """Every type reachable from `roots` through struct/union members (not pNext, not into
        FUNCPOINTER_EXEMPT structures), with the path that reaches it: {type: "A.m -> B.n"}."""
        paths: dict[str, str] = {}
        todo = [(self.resolve(r), self.resolve(r)) for r in roots]
        while todo:
            t, path = todo.pop()
            if t in paths:
                continue
            paths[t] = path
            if self.category(t) in ("struct", "union") and t not in FUNCPOINTER_EXEMPT:
                for m in self.members(t):
                    if m.name == "pNext":
                        continue
                    mt = self.resolve(m.type)
                    if mt not in paths:
                        todo.append((mt, f"{path}.{m.name} -> {mt}"))
        return paths

    def travel(self, d: Decl):
        """(C kind, Rust type) of a generic command's parameter; GenError if it has none."""
        if d.pointer:
            return ("addr", "u64")
        t = self.resolve(d.type)
        cat = self.category(t)
        if cat == "handle":
            return ("addr", "u64")
        if cat == "enum":
            return ("int", "u64" if t in self.bitwidth64 else "u32")
        if cat == "bitmask":
            base = self.types[t].find("type").text
            return ("int", "u64" if base == "VkFlags64" else "u32")
        if cat == "basetype" and t in BASETYPES:
            return BASETYPES[t]
        if cat in ("struct", "union"):
            raise GenError(f"{cat} {t} passed by value (parameter {d.name})")
        if cat is None and t in BASIC_TYPES:
            return BASIC_TYPES[t]
        raise GenError(f"parameter {d.name} has type {d.decl!r} ({cat or 'basic'}) with no forwarding rule")


# ------------------------------------------------------------------------------------------------
# `depends` expressions: names, '+' (and, binds tighter), ',' (or), parentheses.
# ------------------------------------------------------------------------------------------------


def evaluate(expr: str, truth) -> bool:
    toks = re.findall(r"[A-Za-z0-9_]+|[+,()]", expr)
    pos = 0

    def peek():
        return toks[pos] if pos < len(toks) else None

    def take():
        nonlocal pos
        pos += 1
        return toks[pos - 1]

    def atom():
        tok = take()
        if tok == "(":
            v = alternatives()
            if take() != ")":
                raise GenError(f"unbalanced depends expression {expr!r}")
            return v
        return truth(tok)

    def conjunction():
        v = atom()
        while peek() == "+":
            take()
            v = atom() and v
        return v

    def alternatives():
        v = conjunction()
        while peek() == ",":
            take()
            v = conjunction() or v
        return v

    v = alternatives()
    if pos != len(toks):
        raise GenError(f"cannot parse depends expression {expr!r}")
    return v


def names_in(expr: str) -> list[str]:
    return re.findall(r"[A-Za-z0-9_]+", expr)


# ------------------------------------------------------------------------------------------------
# Selection.
# ------------------------------------------------------------------------------------------------


class Selection:
    def __init__(self, reg: Registry, special_names: list[str]):
        self.reg = reg
        self.excluded: dict[str, str] = {}  # extension -> why (only supported="vulkan" ones)
        self.not_vulkan = 0                 # extensions not for Vulkan (disabled, SC-only)
        self.report: list[str] = []
        self._select_extensions()
        self._collect_commands()
        self._classify(special_names)

    # --- extensions ---------------------------------------------------------------------------

    def _truth(self, included):
        def truth(name):
            if name.startswith("VK_VERSION_"):
                return name in CORE_FEATURES
            if name.startswith("VKSC_"):
                return False
            return name in included
        return truth

    def _requires(self, el, included):
        """The <require> blocks of a feature or extension that hold for `included`."""
        truth = self._truth(included)
        for req in el.findall("require"):
            if api_ok(req) and (req.get("depends") is None or evaluate(req.get("depends"), truth)):
                yield req

    def _select_extensions(self):
        reg = self.reg
        included = set()
        for name, e in reg.extensions.items():
            if API not in (e.get("supported") or "").split(","):
                self.not_vulkan += 1
                continue
            platform = e.get("platform")
            if e.get("provisional") == "true" or platform == "provisional":
                self.excluded[name] = "provisional"
            elif platform is not None and platform not in ALLOWED_PLATFORMS:
                self.excluded[name] = f"platform {platform}"
            elif name in EXCLUDED:
                self.excluded[name] = EXCLUDED[name]
            else:
                included.add(name)
        while True:
            changed = False
            truth = self._truth(included)
            for name in sorted(included):
                dep = reg.extensions[name].get("depends") or reg.extensions[name].get("requires")
                if dep and not evaluate(dep, truth):
                    missing = sorted({n for n in names_in(dep) if not truth(n) and not n.startswith("VK_VERSION_")})
                    self.excluded[name] = f"depends on {dep} (without {', '.join(missing)})"
                    included.discard(name)
                    changed = True
            if changed:
                continue
            available = self._available_types(included)
            for name in sorted(included):
                why = self._type_problem(name, included, available)
                if why:
                    self.excluded[name] = why
                    included.discard(name)
                    changed = True
            if not changed:
                break
        self.included = included

    def _available_types(self, included):
        """Types required somewhere: {type: required by a core feature or an included extension}."""
        avail: dict[str, bool] = {}
        self._type_owners: dict[str, set[str]] = {}
        for owner in list(self.reg.features.values()) + list(self.reg.extensions.values()):
            name = owner.get("name")
            self._type_owners[name] = {t.get("name") for req in owner.findall("require") for t in req.findall("type")}
            live = name in CORE_FEATURES or name in included
            for req in owner.findall("require"):
                if not api_ok(req):
                    continue
                ok = live and (req.get("depends") is None or evaluate(req.get("depends"), self._truth(included)))
                for t in req.findall("type"):
                    tn = self.reg.resolve(t.get("name"))
                    avail[tn] = avail.get(tn, False) or ok
        return avail

    def _type_problem(self, name, included, available):
        """Why extension `name` cannot be forwarded because of the types it uses, or None."""
        reg = self.reg
        e = reg.extensions[name]
        roots = []
        for req in self._requires(e, included):
            for c in req.findall("command"):
                cmd = reg.commands[reg.cmd_alias.get(c.get("name"), c.get("name"))]
                roots += [(p.type, f"{cmd.name}({p.name})") for p in cmd.params]
            for t in req.findall("type"):
                if reg.category(t.get("name")) in ("struct", "union"):
                    roots.append((t.get("name"), t.get("name")))
        allowed = PLATFORM_TYPES_ALLOWED.get(e.get("platform"), ())
        for root, where in sorted(set(roots)):
            for t, path in sorted(reg.reach([root]).items()):
                via = f"{where}: {path}" if path != root else where
                if reg.category(t) == "funcpointer":
                    return f"uses function pointer {t} ({via})"
                if reg.is_platform_type(t) and t not in allowed:
                    return f"uses platform type {t} ({via})"
                if reg.category(t) == "handle" and available.get(t) is False:
                    return f"uses handle {t}, which only excluded extensions define ({via})"
        return None

    # --- commands -----------------------------------------------------------------------------

    def _collect_commands(self):
        reg = self.reg
        self.names: dict[str, set[str]] = {}   # canonical -> required names (canonical and aliases)
        self.origin: dict[str, set[str]] = {}  # canonical -> features/extensions requiring it
        owners = [reg.features[f] for f in CORE_FEATURES] + [reg.extensions[n] for n in sorted(self.included)]
        for owner in owners:
            for req in self._requires(owner, self.included):
                for c in req.findall("command"):
                    n = c.get("name")
                    canon = reg.cmd_alias.get(n, n)
                    if canon not in reg.commands:
                        raise GenError(f"{n}: no definition in vk.xml")
                    self.names.setdefault(canon, set()).add(n)
                    self.origin.setdefault(canon, set()).add(owner.get("name"))
        # Every command must be free of function pointers and foreign platform types (core too).
        for canon in sorted(self.names):
            cmd = reg.commands[canon]
            platforms = {reg.extensions[o].get("platform") for o in self.origin[canon] if o in reg.extensions}
            allowed = {t for p in platforms for t in PLATFORM_TYPES_ALLOWED.get(p, ())}
            for p in cmd.params:
                for t, path in reg.reach([p.type]).items():
                    if reg.category(t) == "funcpointer" or (reg.is_platform_type(t) and t not in allowed):
                        raise GenError(f"{canon}: parameter {p.name} reaches {t} ({path})")

    def _classify(self, special_names):
        reg = self.reg
        self.special: set[str] = set()
        for n in special_names:
            canon = reg.cmd_alias.get(n, n)
            if canon not in self.names:
                raise GenError(f"special.txt names {n}, which is not a forwarded command")
            self.special.add(canon)
        self.ids = sorted(self.names)
        for canon in self.ids:
            if canon in self.special:
                continue
            cmd = reg.commands[canon]
            android = [o for o in self.origin[canon] if o in reg.extensions and reg.extensions[o].get("platform") == "android"]
            if android:
                raise GenError(f"{canon}: a command of {android[0]} must be special (add it to tools/vk/special.txt)")
            self.check_generic(cmd)
        for name in sorted(n for n in reg.types if reg.category(n) == "struct" and reg.types[n].get("structextends")):
            if reg.types[name].get("alias"):
                continue
            hits = sorted(path for t, path in reg.reach([name]).items() if reg.is_dispatchable(t))
            if hits:
                owners = sorted(o for o, ts in self._type_owners.items() if name in ts)
                state = ", ".join(f"{o}{'' if o in CORE_FEATURES or o in self.included else ' (excluded)'}" for o in owners)
                self.report.append(f"{name} (extends {reg.types[name].get('structextends')}; {state}): {'; '.join(hits)}")

    def check_generic(self, cmd: Command):
        reg = self.reg
        fix = "(add it to tools/vk/special.txt or exclude its extension)"
        if not cmd.params or cmd.params[0].pointer or not reg.is_dispatchable(cmd.params[0].type):
            raise GenError(f"{cmd.name}: a global command (first parameter not a dispatchable handle) must be special {fix}")
        for p in cmd.params[1:]:
            if reg.is_dispatchable(p.type):
                raise GenError(f"{cmd.name}: parameter {p.name} ({p.decl}) is a dispatchable handle {fix}")
        for p in cmd.params:
            for t, path in sorted(reg.reach([p.type]).items()):
                if t != reg.resolve(p.type) and reg.is_dispatchable(t):
                    raise GenError(f"{cmd.name}: parameter {p.name}'s structure has a dispatchable-handle member ({path}) {fix}")
        for p in cmd.params[1:]:
            try:
                reg.travel(p)
            except GenError as err:
                raise GenError(f"{cmd.name}: {err} {fix}") from None
        if cmd.ret_decl not in RETURNS:
            raise GenError(f"{cmd.name}: return type {cmd.ret_decl!r} has no forwarding rule {fix}")

    def level(self, canon: str) -> int:
        cmd = self.reg.commands[canon]
        first = self.reg.resolve(cmd.params[0].type) if cmd.params and not cmd.params[0].pointer else None
        if first in ("VkInstance", "VkPhysicalDevice"):
            return 1
        if first in ("VkDevice", "VkQueue", "VkCommandBuffer"):
            return 2
        return 0


# ------------------------------------------------------------------------------------------------
# Output.
# ------------------------------------------------------------------------------------------------


def screaming(name: str) -> str:
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name)
    s = re.sub(r"([A-Z])([A-Z][a-z])", r"\1_\2", s)
    return s.upper()


def emit_rust(sel: Selection) -> str:
    reg = sel.reg
    out = [f"// {BANNER}", "//",
           "// One id per forwarded Vulkan command (canonical names, sorted), what each takes, and a generic",
           "// command's call into the host driver through its exact vk.xml signature. The arguments are the",
           "// guest's as 64-bit values: a guest pointer is a host pointer and Vulkan's structs have one layout",
           "// on both sides, so the host driver reads them where they lie; the dispatchable first parameter is",
           "// unwrapped by `Gpu::dispatchable`. SAFETY (every `unsafe` below): the entry point is the host",
           "// driver's for this command (resolved by name from the table of the handle's instance or device),",
           "// called with the command's C signature and the guest's arguments -- guest code already runs in",
           "// this process with every host page reachable, so what the driver reads widens nothing (design).",
           "#![allow(clippy::too_many_lines, clippy::cast_possible_truncation, clippy::cast_possible_wrap, "
           "clippy::cast_sign_loss, clippy::many_single_char_names, clippy::missing_safety_doc, non_upper_case_globals, dead_code)]",
           "use std::ffi::CStr;", "", "use super::{CallError, Gpu};", "use crate::process::Process;", ""]
    for i, canon in enumerate(sel.ids):
        out.append(f"pub(crate) const ID_{screaming(canon)}: u32 = {i};")
    out += ["", "/// (canonical name, argc, special) by id.", "pub(crate) const COMMANDS: &[(&str, u32, bool)] = &["]
    for canon in sel.ids:
        special = "true" if canon in sel.special else "false"
        out.append(f'    ("{canon}", {len(reg.commands[canon].params)}, {special}),')
    out += ["];", "",
            "/// Every extension forwarded to the guest (name, device extension), sorted: the allowlist the",
            "/// host filters what it offers through.",
            "pub(crate) const EXTENSIONS: &[(&str, bool)] = &["]
    for name in sorted(sel.included):
        out.append(f'    ("{name}", {"true" if reg.extensions[name].get("type") == "device" else "false"}),')
    out += ["];", "",
            "pub(crate) fn dispatch(g: &Gpu, p: &Process, id: u32, a: &[u64]) -> Result<u64, CallError> {",
            "    let Some(&(_, argc, special)) = COMMANDS.get(id as usize) else { return Err(CallError::Args) };",
            "    if a.len() != argc as usize {", "        return Err(CallError::Args);", "    }",
            "    if special {", "        return super::special::call(g, p, id, a);", "    }", "    match id {"]
    generic = [c for c in sel.ids if c not in sel.special]
    for canon in generic:
        out.append(f"        ID_{screaming(canon)} => {screaming(canon).lower()}(g, p, a),")
    out += ["        _ => Err(CallError::Args),", "    }", "}"]
    conv = {"u64": "a[{i}]", "i64": "a[{i}] as i64", "u32": "a[{i}] as u32", "i32": "a[{i}] as u32 as i32",
            "u16": "a[{i}] as u16", "i16": "a[{i}] as u16 as i16", "u8": "a[{i}] as u8", "i8": "a[{i}] as u8 as i8",
            "f32": "f32::from_bits(a[{i}] as u32)", "f64": "f64::from_bits(a[{i}])"}
    for canon in generic:
        cmd = reg.commands[canon]
        names = [canon] + sorted(n for n in sel.names[canon] if n != canon)
        tys = ["u64"] + [reg.travel(p)[1] for p in cmd.params[1:]]
        args = ["h0"] + [conv[t].format(i=i) for i, t in enumerate(tys) if i > 0]
        ret = RETURNS[cmd.ret_decl]
        rty = {"void": "", "result": " -> i32", "u32": " -> u32", "u64": " -> u64"}[ret]
        ident = screaming(canon)
        call = f"f({', '.join(args)})"
        out += ["",
                f"fn {ident.lower()}(g: &Gpu, p: &Process, a: &[u64]) -> Result<u64, CallError> {{",
                f"    const NAMES: &[&CStr] = &[{', '.join(f'c{chr(34)}{n}{chr(34)}' for n in names)}];",
                "    let (h0, t) = g.dispatchable(p, a[0])?;",
                f"    let f: unsafe extern \"system\" fn({', '.join(tys)}){rty} = unsafe {{ std::mem::transmute(t.get(ID_{ident}, NAMES)?) }};"]
        if ret == "void":
            out += [f"    unsafe {{ {call} }};", "    Ok(0)"]
        else:
            out.append(f"    let r = unsafe {{ {call} }};")
            out.append({"result": "    Ok(u64::from(r as u32))", "u32": "    Ok(u64::from(r))", "u64": "    Ok(r)"}[ret])
        out.append("}")
    return "\n".join(out) + "\n"


def c_params(cmd: Command) -> str:
    return ", ".join(p.decl for p in cmd.params) or "void"


def emit_guest_h(sel: Selection) -> str:
    reg = sel.reg
    out = [f"/* {BANNER} */",
           "/* The forwarded commands' ids (the host's crates/omni-linux/src/gpu/generated.rs numbers them the",
           " * same), the entry table, and the prototypes of the special (hand-written) entry points. */",
           "#ifndef OMNI_VK_GENERATED_H", "#define OMNI_VK_GENERATED_H", "",
           "/* VK_ANDROID_external_memory_android_hardware_buffer's commands are declared only with this. */",
           "#ifndef VK_USE_PLATFORM_ANDROID_KHR", "#define VK_USE_PLATFORM_ANDROID_KHR", "#endif",
           "#include <stdint.h>", "#include <vulkan/vulkan.h>", "",
           "#if !defined(VK_HEADER_VERSION) || VK_HEADER_VERSION != " + VK_XML_VERSION.rsplit(".", 1)[1],
           f'#error "generated from vk.xml {VK_XML_VERSION}; the Vulkan headers differ"', "#endif",
           "#ifndef VK_ANDROID_external_memory_android_hardware_buffer",
           '#error "<vulkan/vulkan.h> was included without VK_USE_PLATFORM_ANDROID_KHR: include this header first"',
           "#endif", ""]
    for i, canon in enumerate(sel.ids):
        out.append(f"#define OMNI_VK_ID_{screaming(canon)} {i}u")
    out += ["#define OMNI_VK_COMMAND_COUNT " + f"{len(sel.ids)}u", "",
            "/* A name the driver answers GetInstanceProcAddr/GetDeviceProcAddr with. level: 0 global,",
            " * 1 instance (first parameter VkInstance or VkPhysicalDevice), 2 device (VkDevice, VkQueue,",
            " * VkCommandBuffer). */",
            "struct omni_vk_entry {", "    const char* name;", "    PFN_vkVoidFunction fn;", "    uint8_t level;", "};",
            "/* Every forwarded command and alias name, sorted by strcmp (bsearch-able). */",
            "extern const struct omni_vk_entry omni_vk_entries[];", "extern const unsigned omni_vk_entry_count;", "",
            "/* The transport (driver.c): command `id` with `argc` 64-bit arguments; its result. */",
            "uint64_t omni_vk_call(uint32_t id, const uint64_t* args, uint32_t argc);", "",
            "/* The special entry points (tools/vk/special.txt), written by hand. */"]
    for canon in sel.ids:
        if canon in sel.special:
            cmd = reg.commands[canon]
            out.append(f"VKAPI_ATTR {cmd.ret_decl} VKAPI_CALL omni_{canon}({c_params(cmd)});")
    out += ["", "#endif /* OMNI_VK_GENERATED_H */"]
    return "\n".join(out) + "\n"


def emit_guest_c(sel: Selection) -> str:
    reg = sel.reg
    out = [f"/* {BANNER} */",
           "/* Every generic forwarded command: its arguments widened to 64 bits (pointers and handles as",
           " * addresses, integers as values, floats as their bits) and sent to the host, which calls the host",
           " * driver with the command's exact signature. Then the name table. */",
           '#include "generated.h"', "", "#include <string.h>"]
    for canon in sel.ids:
        if canon in sel.special:
            continue
        cmd = reg.commands[canon]
        n = len(cmd.params)
        locals_ = {p.name for p in cmd.params}
        assert not locals_ & {"omni_a", "omni_r"} and not any(x.startswith("omni_bits") for x in locals_), canon
        body = [f"    uint64_t omni_a[{n}];"]
        for i, p in enumerate(cmd.params):
            kind = "addr" if i == 0 else reg.travel(p)[0]
            if kind == "addr":
                body.append(f"    omni_a[{i}] = (uint64_t)(uintptr_t){p.name};")
            elif kind == "int":
                body.append(f"    omni_a[{i}] = (uint64_t){p.name};")
            else:
                w = "uint32_t" if kind == "f32" else "uint64_t"
                body += [f"    {w} omni_bits{i};", f"    memcpy(&omni_bits{i}, &{p.name}, sizeof omni_bits{i});",
                         f"    omni_a[{i}] = omni_bits{i};"]
        call = f"omni_vk_call(OMNI_VK_ID_{screaming(canon)}, omni_a, {n})"
        ret = RETURNS[cmd.ret_decl]
        if ret == "void":
            body.append(f"    (void){call};")
        else:
            body.append(f"    uint64_t omni_r = {call};")
            body.append({"result": "    return (VkResult)(int32_t)(uint32_t)omni_r;",
                         "u32": f"    return ({cmd.ret_decl})omni_r;", "u64": "    return omni_r;"}[ret])
        out += ["", f"static VKAPI_ATTR {cmd.ret_decl} VKAPI_CALL omni_{canon}({c_params(cmd)}) {{"] + body + ["}"]
    entries = sorted((n, canon) for canon, names in sel.names.items() for n in names)
    out += ["", "const struct omni_vk_entry omni_vk_entries[] = {"]
    for n, canon in entries:
        out.append(f'    {{"{n}", (PFN_vkVoidFunction)omni_{canon}, {sel.level(canon)}}},')
    out += ["};", f"const unsigned omni_vk_entry_count = {len(entries)}u;"]
    return "\n".join(out) + "\n"


# ------------------------------------------------------------------------------------------------


def read_special(path: str) -> list[str]:
    names = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if line:
                names.append(line)
    return names


def write(path: str, text: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def report(sel: Selection, out=sys.stderr) -> None:
    reg = sel.reg
    aliases = sum(len(v - {k}) for k, v in sel.names.items())
    special = len(sel.special)
    print(f"gen_vk_forward: vk.xml {VK_XML_VERSION}: {len(sel.ids)} commands "
          f"({len(sel.ids) - special} generic, {special} special), {aliases} alias names, "
          f"{len(sel.included)} extensions forwarded; max argc "
          f"{max(len(reg.commands[c].params) for c in sel.ids)}", file=out)
    print(f"excluded extensions ({len(sel.excluded)}; plus {sel.not_vulkan} not for Vulkan at all):", file=out)
    for name in sorted(sel.excluded):
        print(f"  {name}: {sel.excluded[name]}", file=out)
    print("pNext structures (structextends) with a dispatchable-handle member -- for special code:", file=out)
    for line in sel.report:
        print(f"  {line}", file=out)


def generate(xml: str, special: str, out_root: str, quiet: bool = False) -> Selection:
    sel = Selection(Registry(xml), read_special(special))
    write(os.path.join(out_root, HOST_OUT), emit_rust(sel))
    write(os.path.join(out_root, GUEST_H_OUT), emit_guest_h(sel))
    write(os.path.join(out_root, GUEST_C_OUT), emit_guest_c(sel))
    if not quiet:
        report(sel)
    return sel


def main(argv=None) -> int:
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--xml", default=os.path.join(repo, "tools", "vk", "vk.xml"))
    ap.add_argument("--special", default=os.path.join(repo, "tools", "vk", "special.txt"))
    ap.add_argument("--out-root", default=repo, help="the repository root the outputs are written under")
    ap.add_argument("--quiet", action="store_true", help="no report on stderr")
    args = ap.parse_args(argv)
    try:
        generate(args.xml, args.special, args.out_root, args.quiet)
    except GenError as err:
        print(f"gen_vk_forward: error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
