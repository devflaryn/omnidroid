"""omnidroid on Modal: build once into an image, run Roblox headless, bring back a screenshot.

The notebook (`omnidroid_headless.ipynb`) also runs inside a Modal *notebook*; this is the other
way to use Modal -- a function you call from your own terminal, with the build baked into the
image so every run after the first starts in seconds.

Once:

    pip install modal && modal setup
    modal volume create omnidroid
    modal volume put omnidroid Roblox-2.738.1397.apk /Roblox-2.738.1397.apk
    modal secret create roblox-cookie ROBLOSECURITY="$(cat my-cookie.txt)"

Then (the first run builds the image: 30-60 min; later runs reuse it):

    modal run notebooks/modal_app.py                          # CPU only (Mesa llvmpipe)
    OMNI_MODAL_GPU=T4 modal run notebooks/modal_app.py        # asks for a GPU as well
    modal run notebooks/modal_app.py --place 8737899170 --out shot

It writes `shot.png` and `shot.jpg` beside you and prints the JPEG as a `data:` URL.

Settings, read from your environment when you run `modal run`:

    OMNI_REPO_URL        default https://github.com/devflaryn/omnidroid
    OMNI_BRANCH          default unified
    OMNI_COMMIT          pin a commit (default: the branch's head, looked up with git ls-remote)
    OMNI_GITHUB_SECRET   a Modal secret holding GITHUB_TOKEN, for a private repository
    OMNI_COOKIE_SECRET   default roblox-cookie (a Modal secret holding ROBLOSECURITY)
    OMNI_VOLUME          default omnidroid (holds the APK; the app's storage is kept there too)
    OMNI_MODAL_GPU       e.g. T4, L4, A10G, A100, H100 (default: none)
    OMNI_MODAL_REGION    e.g. eu, us-east (default: Modal's choice) -- see the cookie warning
    OMNI_ACCOUNT_COUNTRY the cookie's country (e.g. TR): the run stops before using the cookie
                         when Modal's exit country differs

**The cookie.** Roblox can end a session whose cookie arrives from another country -- for good.
Modal's machines are in the US, Europe and a few other regions; use an account that is used from
there, and set OMNI_ACCOUNT_COUNTRY to be stopped instead of finding out. The cookie is never
printed.

**GPU.** Modal's sandbox (gVisor) exposes NVIDIA's compute libraries, and may not expose its EGL
(graphics) library; without it the game renders on the CPU with llvmpipe, which is slow but
enough for a screenshot. The function says which renderer it got.
"""

import os
import subprocess

import modal

REPO_URL = os.environ.get("OMNI_REPO_URL", "https://github.com/devflaryn/omnidroid")
BRANCH = os.environ.get("OMNI_BRANCH", "unified")
GITHUB_SECRET = os.environ.get("OMNI_GITHUB_SECRET", "")
COOKIE_SECRET = os.environ.get("OMNI_COOKIE_SECRET", "roblox-cookie")
VOLUME = os.environ.get("OMNI_VOLUME", "omnidroid")
GPU = os.environ.get("OMNI_MODAL_GPU") or None
REGION = os.environ.get("OMNI_MODAL_REGION") or None
ACCOUNT_COUNTRY = os.environ.get("OMNI_ACCOUNT_COUNTRY", "")
RUST_TOOLCHAIN = "1.98.1"
# The stock Roblox 2.738.1397 APK; any other file is refused (a modified build given a cookie can
# take the account). OMNI_APK_SHA256 names another stock build, comma-separated.
APK_SHA256 = os.environ.get("OMNI_APK_SHA256", "bbe00ae306cc251c4ea55b7a932d9c524ecb0d6d9203c2a6161bcf0fae792742")
SRC = "/opt/omnidroid"
MNT = "/mnt/omnidroid"
RUN_DIR = "/root/run"


def _branch_head():
    """The commit the image is built from: part of the build step, so a new commit rebuilds."""
    if os.environ.get("OMNI_COMMIT"):
        return os.environ["OMNI_COMMIT"]
    if not modal.is_local():
        return "in-container"
    try:
        out = subprocess.run(["git", "ls-remote", REPO_URL, f"refs/heads/{BRANCH}"], capture_output=True,
                             text=True, timeout=60)
        return out.stdout.split()[0] if out.stdout.strip() else BRANCH
    except (OSError, subprocess.TimeoutExpired):
        return BRANCH


COMMIT = _branch_head()

APT_PACKAGES = [
    "build-essential", "g++-12", "cmake", "ninja-build", "pkg-config", "git", "curl", "ca-certificates", "zstd",
    "libasound2-dev", "libx11-dev", "libxi-dev", "libxfixes-dev",
    "libvulkan1", "libvulkan-dev", "mesa-vulkan-drivers",
    "libegl1", "libgles2", "libegl-mesa0", "libgl1-mesa-dri", "libglvnd0",
    "xvfb", "x11-xkb-utils", "xkb-data", "xauth", "imagemagick", "x11-utils",
]

BUILD_ENV = {
    "DEBIAN_FRONTEND": "noninteractive",
    "PATH": "/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "RUSTUP_TOOLCHAIN": RUST_TOOLCHAIN,
    # Ubuntu 22.04's g++ is 11; dynarmic is C++20. The same variables at run time, or
    # `omnidroid play`'s `cargo test` would rebuild dynarmic.
    "CC": "gcc-12",
    "CXX": "g++-12",
    "CARGO_TERM_COLOR": "never",
    # Ask for NVIDIA's graphics libraries too, where the sandbox can give them.
    "NVIDIA_DRIVER_CAPABILITIES": "all",
}


def build_omnidroid():
    """Runs once, while the image is built: the launcher and the runtime it starts."""
    for args in (["cargo", "build", "--release", "-p", "omnidroid"],
                 ["cargo", "test", "--release", "-p", "omni-android", "--test", "gameactivity", "--no-run"]):
        subprocess.run(args, cwd=SRC, check=True)


clone = (f"git clone --depth 1 --branch {BRANCH} "
         + (f"https://x-access-token:$GITHUB_TOKEN@{REPO_URL.removeprefix('https://')}" if GITHUB_SECRET else REPO_URL)
         + f" {SRC} && git -C {SRC} remote set-url origin {REPO_URL}"
         + f" && echo 'wanted {COMMIT}, got' $(git -C {SRC} rev-parse HEAD)")

image = (
    modal.Image.from_registry("ubuntu:22.04", add_python="3.12")
    .env(BUILD_ENV)
    .apt_install(*APT_PACKAGES)
    .run_commands(f"curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal "
                  f"--default-toolchain {RUST_TOOLCHAIN} --no-modify-path")
    .pip_install("pillow")
    .run_commands(clone, secrets=[modal.Secret.from_name(GITHUB_SECRET)] if GITHUB_SECRET else [])
    .run_function(build_omnidroid, cpu=8.0, memory=16384, timeout=3 * 3600)
)

app = modal.App("omnidroid-headless", image=image)
volume = modal.Volume.from_name(VOLUME, create_if_missing=True)


# ---- what runs in the container ------------------------------------------------------------

def _sh(args, **kw):
    return subprocess.run(args, capture_output=True, text=True, **kw)


def _graphics(headless):
    """(environment, description): NVIDIA EGL when it is there and the run is headless, else llvmpipe."""
    import glob
    import json
    import shutil
    gpu = ""
    if shutil.which("nvidia-smi"):
        gpu = _sh(["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"]).stdout.strip().replace("\n", ", ")
    nvidia_egl = None
    for line in _sh(["ldconfig", "-p"]).stdout.splitlines():
        if line.strip().startswith("libEGL_nvidia.so.0 ") and "x86-64" in line:
            nvidia_egl = line.split("=>")[-1].strip()
    env = {}
    lvp = sorted(glob.glob("/usr/share/vulkan/icd.d/lvp_icd*.json"))
    if lvp:  # lavapipe only: the engine skips an emulated Vulkan device and draws with GLES
        env.update(VK_DRIVER_FILES=":".join(lvp), VK_ICD_FILENAMES=":".join(lvp))
    mesa_json = next(iter(glob.glob("/usr/share/glvnd/egl_vendor.d/*mesa*.json")), "")
    if gpu and nvidia_egl and headless:
        os.makedirs("/tmp/egl", exist_ok=True)
        with open("/tmp/egl/10_nvidia.json", "w") as f:
            json.dump({"file_format_version": "1.0.0", "ICD": {"library_path": nvidia_egl}}, f)
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = f"/tmp/egl/10_nvidia.json:{mesa_json}"
        return env, f"GPU ({gpu}) through NVIDIA EGL"
    env.update(LIBGL_ALWAYS_SOFTWARE="1", GALLIUM_DRIVER="llvmpipe", __EGL_VENDOR_LIBRARY_FILENAMES=mesa_json)
    why = ("no GPU" if not gpu else "the GPU's EGL library is not exposed" if not nvidia_egl
           else "this build has no --headless (Xvfb)")
    return env, f"CPU (Mesa llvmpipe): {why}" + (f"; GPU present: {gpu}" if gpu else "")


def _exit_country():
    import urllib.request
    try:
        trace = urllib.request.urlopen("https://www.cloudflare.com/cdn-cgi/trace", timeout=15).read().decode()
        return dict(l.split("=", 1) for l in trace.splitlines() if "=" in l).get("loc", "?")
    except OSError:
        return "?"


@app.function(gpu=GPU, cpu=4.0, memory=12288, timeout=2 * 3600, region=REGION,
              volumes={MNT: volume}, secrets=[modal.Secret.from_name(COOKIE_SECRET)])
def play_and_screenshot(place_id: int = 8737899170, settle_seconds: int = 60, load_timeout_minutes: int = 30,
                        account: str = "modal") -> dict:
    import glob
    import re
    import shutil
    import signal
    import time

    t0 = time.time()
    notes = []

    def note(text):
        line = f"[{int(time.time() - t0) // 60}:{int(time.time() - t0) % 60:02d}] {text}"
        print(line, flush=True)
        notes.append(line)

    country = _exit_country()
    note(f"exit country {country}")
    if ACCOUNT_COUNTRY and country != "?" and country.upper() != ACCOUNT_COUNTRY.upper():
        raise RuntimeError(f"the cookie is from {ACCOUNT_COUNTRY} and this machine is in {country}: stopped before "
                           "using it (Roblox may end its session). Pick another OMNI_MODAL_REGION or account.")
    cookie = os.environ.get("ROBLOSECURITY", "").strip()
    if len(cookie) < 100:
        raise RuntimeError(f"the Modal secret {COOKIE_SECRET!r} has no ROBLOSECURITY value")
    work = RUN_DIR
    os.makedirs(f"{work}/secrets", mode=0o700, exist_ok=True)
    cookie_path = f"{work}/secrets/{account}.txt"
    fd = os.open(cookie_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(cookie + "\n")
    del cookie

    apks = sorted(glob.glob(f"{MNT}/*.apk") + glob.glob(f"{MNT}/*/*.apk"), key=os.path.getsize)
    if not apks:
        raise RuntimeError(f"no APK in the volume {VOLUME!r}: modal volume put {VOLUME} <the .apk> /")
    import hashlib

    def sha256_of(path):
        digest = hashlib.sha256()
        with open(path, "rb") as f:
            for block in iter(lambda: f.read(1 << 20), b""):
                digest.update(block)
        return digest.hexdigest()

    allowed = {h.strip().lower() for h in APK_SHA256.split(",") if h.strip()}
    source = next((p for p in apks if sha256_of(p) in allowed), apks[-1])
    apk = f"{work}/{os.path.basename(source)}"
    shutil.copyfile(source, apk)
    digest = sha256_of(apk)
    if digest not in allowed:
        os.unlink(apk)
        raise RuntimeError(f"REFUSED: {source} has sha256 {digest}, not the stock build's {' or '.join(sorted(allowed))}")
    note(f"APK {os.path.basename(apk)} ({os.path.getsize(apk) >> 20} MiB), sha256 {digest} -- the stock build")

    # The app's storage lives on the volume between runs (a rotated cookie is kept there); it is
    # used from local disk and copied back at the end, without the APK the runtime links into it.
    data = f"{work}/data/{account}"
    kept = f"{MNT}/data/{account}"
    if os.path.isdir(kept):
        shutil.copytree(kept, data, dirs_exist_ok=True)
    os.makedirs(data, exist_ok=True)

    omnidroid = f"{SRC}/target/release/omnidroid"
    usage = _sh([omnidroid, "--help"]).stdout
    headless, has_control = "--headless" in usage, "--control" in usage
    gfx, renderer = _graphics(headless)
    note(f"{'headless' if headless else 'Xvfb'}; rendering on the {renderer}")

    env = dict(os.environ, **gfx, OMNI_WINDOW_SIZE="1280x720", OMNI_GRAPHICS_QUALITY="1")
    env.pop("ROBLOSECURITY", None)
    cmd = [omnidroid, "play", "--apk", apk, "--cookie", cookie_path, "--place", str(place_id),
           "--join-delay", "20" if "GPU" in renderer.split(":")[0] else "45", "--minutes", "90", "--data-dir", data]
    control = f"{work}/control.txt"
    open(control, "w").close()
    xvfb = None
    if headless:
        cmd.append("--headless")
        if "--no-window" in usage:  # no display: an off-screen EGL surface, GLES
            cmd.append("--no-window")
        env.pop("DISPLAY", None)
    else:
        xvfb = subprocess.Popen(["Xvfb", ":99", "-screen", "0", "1280x720x24", "-nolisten", "tcp", "-extension", "GLX"],
                                env=dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1"), stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)
        time.sleep(2)
        env["DISPLAY"] = ":99"
    if has_control:
        cmd += ["--control", control]
    log_path = f"{work}/game.log"
    game = subprocess.Popen(cmd, env=env, cwd=work, stdin=subprocess.DEVNULL, stdout=open(log_path, "w"),
                            stderr=subprocess.STDOUT, start_new_session=True)

    def log_from(pos):
        with open(log_path, errors="replace") as f:
            f.seek(pos)
            return f.read().splitlines(), f.tell()

    frames_re = re.compile(r"^FRAMES: \+(\d+)s into the session, (\d+) presents \(\+(\d+) in the last (\d+)s\)")
    marks = {"DID_LOG_IN": "signed in", "JOIN: nativeAppBridgeV2StartGameWithParam": "joining",
             f"onGameLoaded: placeId:{place_id}": "the place has loaded; its loading screen is up", "GL Renderer:": None}
    shot = f"{work}/shot.png"

    def send(command, expect, timeout=120):
        start = os.path.getsize(log_path)
        with open(control, "a") as f:
            f.write(command + "\n")
        deadline = time.time() + timeout
        while time.time() < deadline:
            for line in log_from(start)[0]:
                if any(e in line for e in ([expect] if isinstance(expect, str) else expect)):
                    return line.strip()
            time.sleep(0.5)
        raise TimeoutError(f"no {expect!r} after {command!r}")

    def take_screenshot():
        if has_control:  # the frame is rendered for real even while headless
            answer = send(f"screenshot {shot}", ("SCREENSHOT: saved", "SCREENSHOT: failed"))
            if "failed" in answer:
                raise RuntimeError(answer)
        else:
            subprocess.run(["import", "-display", ":99", "-window", "root", shot], check=True)

    def flatness():
        """The share of the screen in its most common colour: a loading screen ~95%, the world ~20%."""
        from PIL import Image
        img = Image.open(shot).convert("RGB").resize((320, 180))
        return max(n for n, _ in img.point(lambda v: v & 0xF0).getcolors(320 * 180)) / (320 * 180)

    seen, pos, loaded, fps, next_look = set(), 0, None, None, 0
    png = jpeg = None
    try:
        while True:
            lines, pos = log_from(pos)
            for line in lines:
                for mark, what in marks.items():
                    if mark in line and mark not in seen:
                        seen.add(mark)
                        note(what or line.split("] ", 1)[-1].strip())
                        if mark.startswith("onGameLoaded"):
                            loaded = time.time()
                m = frames_re.match(line)
                if m:
                    fps = int(m.group(3)) / int(m.group(4))
                if "GUEST THREAD DIED" in line or "panicked at" in line:
                    note(line.strip()[:300])
            if game.poll() is not None:
                raise RuntimeError(f"the game exited ({game.returncode}) before it was on screen:\n"
                                   + "\n".join(log_from(max(0, os.path.getsize(log_path) - 4000))[0][-30:]))
            now = time.time()
            if loaded and now - loaded >= settle_seconds and now >= next_look:
                next_look = now + 20
                take_screenshot()
                share = flatness()
                if share < 0.6:
                    note(f"in the world ({share:.0%} of the screen one colour"
                         + (f", {fps:.1f} frames/s)" if fps is not None else ")"))
                    break
                note(f"still a loading screen ({share:.0%} one colour)")
            if now - t0 > load_timeout_minutes * 60:
                if loaded and os.path.exists(shot):
                    note(f"still a loading screen after {load_timeout_minutes} minutes; returning it anyway")
                    break
                raise TimeoutError(f"the place did not load in {load_timeout_minutes} minutes")
            time.sleep(2)

        from PIL import Image
        import io
        png = open(shot, "rb").read()
        img = Image.open(io.BytesIO(png)).convert("RGB")
        img.thumbnail((1280, 720))
        buf = io.BytesIO()
        img.save(buf, "JPEG", quality=80, optimize=True)
        jpeg = buf.getvalue()
    finally:
        if game.poll() is None:
            os.killpg(game.pid, signal.SIGINT)
            try:
                game.wait(30)
            except subprocess.TimeoutExpired:
                os.killpg(game.pid, signal.SIGKILL)
        if xvfb:
            xvfb.terminate()
        shutil.copytree(data, kept, dirs_exist_ok=True, ignore=shutil.ignore_patterns("*.apk"))
        volume.commit()
    return {"png": png, "jpeg": jpeg, "notes": notes, "renderer": renderer, "country": country}


@app.local_entrypoint()
def main(place: int = 8737899170, out: str = "omnidroid-shot", settle: int = 60):
    import base64
    result = play_and_screenshot.remote(place, settle)
    with open(f"{out}.png", "wb") as f:
        f.write(result["png"])
    with open(f"{out}.jpg", "wb") as f:
        f.write(result["jpeg"])
    print("\n".join(result["notes"]))
    print(f"saved {out}.png and {out}.jpg (rendered on the {result['renderer']})")
    print("Paste this whole line into a browser's address bar:")
    print("data:image/jpeg;base64," + base64.b64encode(result["jpeg"]).decode())
