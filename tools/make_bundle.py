#!/usr/bin/env python3
"""Build omnidroid and lay it out as an install: a plain folder that runs with no cargo, no Python and
no checkout beside it (`omni_linux::install` says how a program knows it runs from one). The Omni
Executor desktop app ships this folder and runs `omnidroid serve --port <p>` from it.

    python3 tools/make_bundle.py --out <dir> [--zip] [--sysroot <dir>] [-j <n>]

    <out>/omnidroid(.exe)                the launcher, its pool and MCP server
    <out>/window/Omnidroid(.exe)         the device runner: the name the menu bar, Dock and Task Manager show
    <out>/omni-r-app(.exe)               the `r_app` gate, prebuilt: what boots a device
    <out>/omni-plant-cookie(.exe)        tools/plant_cookie.py, ported (crates/omni-cookie)
    <out>/omni-mcp(.exe)                 the MCP stdio bridge and the lab's tools
    <out>/sysroot/aosp-35/...            the pinned sysroot, byte for byte
    <out>/plugins/roblox/omnidroid-plugin.json     its hooks run the prebuilt program, not cargo;
    <out>/plugins/roblox/omnidroid-plugin-roblox(.exe)   `login` (Python and Chromium) is left out
    <out>/BUNDLE.json                    {"version": <git short sha>, "built": <ISO time>, "platform": ...}

`--zip` also writes `<out>.zip` beside the folder (deflate; the folder's contents at the zip's root,
executables keeping their mode for macOS and Linux). The sysroot is taken from `--sysroot` (default:
the checkout's `sysroot/aosp-35`, which is not in git) and checked against the pin in
`crates/omni-linux/src/vfs.rs` before anything is copied; a folder made before is brought up to date
in place (a sysroot object of the same name and size is not copied again: they are named by their
hash). On Windows, dynarmic's build wants a short path: set `OMNIDROID_DYNARMIC_BUILD_DIR=C:\\od-build`
for a checkout deeper than the drive root (dynarmic-sys's build says so when it is needed).
Stdlib only, and the same on Windows, macOS and Linux (no `.exe` off Windows).
"""
import argparse, datetime, hashlib, json, os, platform, re, shutil, subprocess, sys, zipfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = ".exe" if os.name == "nt" else ""
SYSROOT = os.path.join("sysroot", "aosp-35")
# The plugins the bundle comes with, and the commands of theirs it leaves out: `login` signs in in
# Chromium under Python, which an install does not have.
PLUGINS = {"roblox": {"drop_commands": {"login"}}}
# (package, binary) built into the bundle under the binary's own name.
# Where the device runner goes: its file name is what people see it called (crates/omni-linux/src/install.rs).
RUNNER = "window/Omnidroid"
BINARIES = [("omnidroid", "omnidroid"), ("omni-linux", "omni-linux-run"), ("omni-mcp", "omni-mcp"), ("omni-cookie", "omni-plant-cookie")]


def say(text):
    print(f"make_bundle: {text}", flush=True)


def cargo_artifacts(args, cwd=REPO):
    """Run `cargo <args> --message-format=json`: {target name: [executables]} of what it built."""
    cmd = ["cargo"] + args + ["--message-format=json-render-diagnostics"]
    say(" ".join(cmd))
    proc = subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    found = {}
    for line in proc.stdout:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
            found.setdefault(msg["target"]["name"], []).append(msg["executable"])
    if proc.wait() != 0:
        sys.exit(f"make_bundle: cargo {' '.join(args)} failed")
    return found


def one(found, name):
    paths = found.get(name) or []
    if len(paths) != 1:
        sys.exit(f"make_bundle: cargo built {len(paths)} executables named {name}: {paths}")
    return paths[0]


def build(jobs):
    """Every program the bundle holds, built in release: {name in the bundle: built path}."""
    # Compiled code records where it was built, home folder and all, and this bundle is public.
    home = os.path.expanduser("~")
    flags = [f"--remap-path-prefix={home}=" + ("C:\build" if os.name == "nt" else "/build")]
    if os.name == "nt":
        flags += ["-C", "link-arg=/PDBALTPATH:%_PDB%"]
    else:
        for var in ("CFLAGS", "CXXFLAGS"):
            os.environ[var] = f"{os.environ.get(var, '')} -ffile-prefix-map={home}=/build".strip()
    os.environ["RUSTFLAGS"] = " ".join([os.environ.get("RUSTFLAGS", "")] + flags).strip()
    j = ["-j", str(jobs)] if jobs else []
    args = ["build", "--release"] + j
    for package, binary in BINARIES:
        args += ["-p", package, "--bin", binary]
    found = cargo_artifacts(args)
    built = {binary: one(found, binary) for _, binary in BINARIES}
    # The gate is a test binary: `cargo test --no-run` builds it as `omnidroid aosp` in a checkout
    # would have run it, and names where it is.
    tests = cargo_artifacts(["test", "--release", "-p", "omni-linux", "--test", "r_app", "--no-run"] + j)
    built["omni-r-app"] = one(tests, "r_app")
    for name in PLUGINS:
        manifest = os.path.join(REPO, "plugins", name, "Cargo.toml")
        found = cargo_artifacts(["build", "--release", "--manifest-path", manifest] + j)
        built[f"plugin:{name}"] = one(found, f"omnidroid-plugin-{name}")
    return built


def pinned(name):
    """The sha256 `crates/omni-linux/src/vfs.rs` pins as `name`."""
    text = open(os.path.join(REPO, "crates", "omni-linux", "src", "vfs.rs"), encoding="utf-8").read()
    m = re.search(rf'pub const {name}: &str = "([0-9a-f]{{64}})"', text)
    if not m:
        sys.exit(f"make_bundle: no {name} in vfs.rs")
    return m.group(1)


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_sysroot(src):
    for file, pin in [("sysroot.manifest", "SYSROOT_MANIFEST_SHA256"), ("sysroot.meta", "SYSROOT_META_SHA256")]:
        path = os.path.join(src, file)
        if not os.path.isfile(path):
            sys.exit(f"make_bundle: no {path}: the sysroot is made by tools/make_sysroot.py")
        digest = sha256_of(path)
        if digest != pinned(pin):
            sys.exit(f"make_bundle: {path} is sha256 {digest}, not the pinned {pinned(pin)}")


def copy_if_changed(src, dst):
    """Copy `src` to `dst` unless `dst` is there with the same size: True if it was copied."""
    if os.path.isfile(dst) and os.path.getsize(dst) == os.path.getsize(src):
        return False
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    shutil.copyfile(src, dst)
    return True


def copy_sysroot(src, dst):
    """The sysroot, byte for byte: its manifest and meta (always copied anew), its objects, and the
    current boot images it keeps uncompressed (`omni_linux::boot_image`: `<sha>.v<N>.*`; the bare
    ones an older build made are left out). The size marker in `objects/` is the sysroot's own and
    still true of the copy. The objects copied are checked by size against the source."""
    copied = total = 0
    for file in ["sysroot.manifest", "sysroot.meta"]:
        os.makedirs(dst, exist_ok=True)
        shutil.copyfile(os.path.join(src, file), os.path.join(dst, file))
    for sub, keep in [("objects", lambda n: True), ("boot-uncompressed", lambda n: re.search(r"\.v\d+\.(art|sha256)$", n))]:
        root = os.path.join(src, sub)
        for here, _, files in os.walk(root):
            for name in files:
                if not keep(name):
                    continue
                s = os.path.join(here, name)
                d = os.path.join(dst, sub, os.path.relpath(s, root))
                total += 1
                copied += copy_if_changed(s, d)
                if os.path.getsize(d) != os.path.getsize(s):
                    sys.exit(f"make_bundle: {d} is not the size of {s}")
    for file, pin in [("sysroot.manifest", "SYSROOT_MANIFEST_SHA256"), ("sysroot.meta", "SYSROOT_META_SHA256")]:
        if sha256_of(os.path.join(dst, file)) != pinned(pin):
            sys.exit(f"make_bundle: the copied {file} is not the pinned one")
    say(f"sysroot: {total} files, {copied} copied ({total - copied} already there)")


def bundled_manifest(name, text, program):
    """A plugin's manifest as the bundle runs it: every `${CARGO} run ... -- <words>` becomes the
    prebuilt `${PLUGIN_DIR}/<program> <words>`, and the commands the bundle cannot run are gone."""
    manifest = json.loads(text)

    def prebuilt(run):
        if run and run[0] == "${CARGO}":
            if "--" not in run:
                sys.exit(f"make_bundle: plugin {name}: a cargo command with no `--`: {run}")
            return ["${PLUGIN_DIR}/" + program] + run[run.index("--") + 1:]
        return run

    manifest["hooks"] = {event: prebuilt(run) for event, run in manifest.get("hooks", {}).items()}
    commands = {c: v for c, v in manifest.get("commands", {}).items() if c not in PLUGINS[name]["drop_commands"]}
    for command in commands.values():
        command["run"] = prebuilt(command["run"])
    if commands:
        manifest["commands"] = commands
    else:
        manifest.pop("commands", None)
    left = [w for run in list(manifest["hooks"].values()) + [c["run"] for c in commands.values()] for w in run if "${CARGO}" in w]
    if left:
        sys.exit(f"make_bundle: plugin {name} still runs cargo: {left}")
    return json.dumps(manifest, indent=2) + "\n"


def platform_name():
    system = {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}.get(platform.system(), platform.system().lower())
    machine = platform.machine().lower()
    machine = {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    return f"{system}-{machine}"


def git_version():
    try:
        sha = subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], cwd=REPO, text=True).strip()
        dirty = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=REPO).returncode != 0
        return sha + ("-dirty" if dirty else "")
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def write_zip(out):
    path = out.rstrip("/\\") + ".zip"
    tmp = path + ".part"
    n = 0
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED, compresslevel=6, allowZip64=True) as z:
        for here, dirs, files in os.walk(out):
            dirs.sort()
            for name in sorted(files):
                full = os.path.join(here, name)
                info = zipfile.ZipInfo.from_file(full, os.path.relpath(full, out).replace(os.sep, "/"))
                info.compress_type = zipfile.ZIP_DEFLATED
                # The executables' mode (macOS and Linux): an unpacked bundle runs as it is.
                executable = name.endswith(EXE) if EXE else os.access(full, os.X_OK)
                mode = 0o755 if executable else 0o644
                info.external_attr = (0o100000 | mode) << 16
                with open(full, "rb") as f, z.open(info, "w", force_zip64=True) as w:
                    shutil.copyfileobj(f, w, 1 << 20)
                n += 1
    os.replace(tmp, path)
    say(f"{path}: {n} files, {os.path.getsize(path) >> 20} MiB")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", required=True, help="the bundle's folder (made, or brought up to date)")
    ap.add_argument("--zip", action="store_true", help="also write <out>.zip beside it")
    ap.add_argument("--sysroot", default=os.path.join(REPO, SYSROOT), help="the pinned sysroot to copy (default: the checkout's)")
    ap.add_argument("-j", "--jobs", type=int, default=0, help="cargo's -j")
    a = ap.parse_args()
    out = os.path.abspath(a.out)
    if os.path.isdir(out) and os.listdir(out) and not os.path.isfile(os.path.join(out, "BUNDLE.json")):
        sys.exit(f"make_bundle: {out} holds other files (no BUNDLE.json): not written over")
    check_sysroot(a.sysroot)
    built = build(a.jobs)

    os.makedirs(out, exist_ok=True)
    for name, path in built.items():
        if name.startswith("plugin:"):
            continue
        placed = RUNNER + EXE if name == "omni-linux-run" else name + EXE
        os.makedirs(os.path.dirname(os.path.join(out, placed)), exist_ok=True)
        shutil.copy2(path, os.path.join(out, placed))
        say(f"{placed} <- {path}")
    for name in PLUGINS:
        src = os.path.join(REPO, "plugins", name)
        dst = os.path.join(out, "plugins", name)
        shutil.rmtree(dst, ignore_errors=True)
        os.makedirs(dst)
        program = f"omnidroid-plugin-{name}{EXE}"
        shutil.copy2(built[f"plugin:{name}"], os.path.join(dst, program))
        text = open(os.path.join(src, "omnidroid-plugin.json"), encoding="utf-8").read()
        with open(os.path.join(dst, "omnidroid-plugin.json"), "w", encoding="utf-8", newline="\n") as f:
            f.write(bundled_manifest(name, text, program))
        say(f"plugins/{name}: {program} and its manifest")
    copy_sysroot(a.sysroot, os.path.join(out, SYSROOT))
    # Some programs compile in the folder they were built from, so build a release from a checkout
    # outside the home folder (C:\od-src, /tmp/omni-src), and refuse anything that names it.
    home = os.path.expanduser("~").encode()
    for root, dirs, files in os.walk(out):
        dirs[:] = [d for d in dirs if d != "sysroot"]
        for name in files:
            with open(os.path.join(root, name), "rb") as f:
                data = f.read()
            if home in data or home.replace(b"\\", b"/") in data:
                sys.exit(f"make_bundle: {name} contains the home folder; build from a checkout outside it")
    info = {"version": git_version(), "built": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"), "platform": platform_name()}
    with open(os.path.join(out, "BUNDLE.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(info, f, indent=2)
        f.write("\n")
    size = sum(os.path.getsize(os.path.join(h, n)) for h, _, fs in os.walk(out) for n in fs)
    say(f"{out}: {info['version']} for {info['platform']}, {size >> 20} MiB")
    if a.zip:
        write_zip(out)


if __name__ == "__main__":
    main()
