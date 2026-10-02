#!/usr/bin/env python3
"""Time a Roblox session on the host's warm device: from nothing installed to in the place.

    python tools/warm_join.py --apk <apk> --cookie <file> --place 8737899170 [--mode preplant|relaunch]
                              [--label L] [--after S] [--shots S]

The warm device (`omnidroid aosp --warm`) is up and idle; the APK is uninstalled first (not
timed). Then, timed from t0: the APK installed, the cookie put in the app's WebView cookie store,
the app started from its launcher, the place's deep link sent once it signs in. `relaunch` is what
`r_roblox` does on a new device (first start, stop, plant, start again); `preplant` writes the
cookie store before the first start. Every `--shots` seconds the display is copied to
`<scratch>/<label>/<ms>.png`; the log's marks (DID_LOG_IN, Joining game, onGameLoaded...) are
printed with their time since t0, and written to `<scratch>/<label>/marks.json`.

The prototype of `omnidroid aosp` on a warm device (crates/omnidroid/src/warm.rs); time the real
command with tools/join_timer.py (tools/warm_join_bench.ps1). Tried here and not kept
(2026-10-02): `--seed` (the app's caches from an earlier run -- OTA patches, settings, its
content store, even one filled in PS99 -- copied in before the first start: no faster sign-in or
join), `--link-in-start` (the place's link on the launcher's own intent: the app ignores it).
"""
import argparse, json, os, shutil, sqlite3, sys, tempfile, threading, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from device_ctl import find_device, run  # noqa: E402
from ingame_screen import is_loading_screen  # noqa: E402

PKG = "com.roblox.client"
MARKS = ["DID_LOG_IN", "Joining game", "onGameLoaded", "Upgrade required", "GUEST THREAD DIED", "has died",
         "Client has been disconnected", "[zygote] launching com.roblox.client", "ActivityNativeMain",
         "Displayed com.roblox.client"]

START_DATA = " -a android.intent.action.VIEW -d \"roblox://experiences/start?placeId={place}\""

# The app's own caches, account-free: its OTA patches (downloaded, decompressed, verified at the
# first start), its Lua app's cache, its settings and flags, its content store.
SEED = ["files/ota_rbxm_decompressed_cache", "files/appData/OTAPatchBackups", "files/UniversalApp_cache",
        "files/UniversalApp_cache_checksum", "shared_prefs/cached_app_settings_prefs.xml", "shared_prefs/ota_state.xml",
        "shared_prefs/cached_flag_prefs.xml", "cache/rbx-storage",
        "files/appData/rbx-storage.db", "files/appData/rbx-storage.db-wal"]

COOKIES_SCHEMA = [
    "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR)",
    "CREATE TABLE cookies(creation_utc INTEGER NOT NULL,host_key TEXT NOT NULL,top_frame_site_key TEXT NOT NULL,"
    "name TEXT NOT NULL,value TEXT NOT NULL,encrypted_value BLOB NOT NULL,path TEXT NOT NULL,expires_utc INTEGER NOT NULL,"
    "is_secure INTEGER NOT NULL,is_httponly INTEGER NOT NULL,last_access_utc INTEGER NOT NULL,has_expires INTEGER NOT NULL,"
    "is_persistent INTEGER NOT NULL,priority INTEGER NOT NULL,samesite INTEGER NOT NULL,source_scheme INTEGER NOT NULL,"
    "source_port INTEGER NOT NULL,last_update_utc INTEGER NOT NULL)",
    "CREATE UNIQUE INDEX cookies_unique_index ON cookies(host_key, top_frame_site_key, name, path, source_scheme, source_port)",
]


def make_store(path: Path, cookie_file: Path):
    from plant_cookie import value_of, chromium_now
    value = value_of(cookie_file)
    assert value.startswith("_|WARNING"), "not a .ROBLOSECURITY value"
    path.unlink(missing_ok=True)
    c = sqlite3.connect(path, isolation_level=None)
    c.execute("PRAGMA journal_mode=DELETE")
    for s in COOKIES_SCHEMA:
        c.execute(s)
    c.executemany("INSERT INTO meta VALUES (?, ?)", [("mmap_status", "-1"), ("version", "21"), ("last_compatible_version", "21")])
    now, year = chromium_now(), 365 * 24 * 3600 * 1_000_000
    c.execute(
        "INSERT INTO cookies VALUES (?, '.roblox.com', '', '.ROBLOSECURITY', ?, x'', '/', ?, 1, 1, ?, 1, 1, 1, -1, 2, 443, ?)",
        (now, value, now + year, now, now))
    c.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apk", required=True)
    ap.add_argument("--cookie", required=True)
    ap.add_argument("--place", default="8737899170")
    ap.add_argument("--mode", default="preplant", choices=["preplant", "relaunch"])
    ap.add_argument("--label", default=time.strftime("wj-%H%M%S"))
    ap.add_argument("--out", default=os.environ.get("WJ_OUT", tempfile.gettempdir()))
    ap.add_argument("--after", type=float, default=5, help="seconds to keep watching after the in-game loading screen")
    ap.add_argument("--link-at", default="DID_LOG_IN", help="the mark that sends the place's link (DID_LOG_IN, ActivityNativeMain, ...)")
    ap.add_argument("--limit", type=float, default=420)
    ap.add_argument("--shots", type=float, default=1.0)
    ap.add_argument("--idle", type=float, default=20, help="seconds idle after the uninstall (a spare made again)")
    ap.add_argument("--keep", action="store_true", help="leave the app running at the end")
    ap.add_argument("--link-in-start", action="store_true", help="the place's link on the launcher's own start intent")
    ap.add_argument("--seed", help="an app data directory of an earlier run: its caches (SEED) copied in before the start")
    a = ap.parse_args()

    dev = find_device()
    if not dev:
        sys.exit("no live warm device")
    out = Path(a.out) / a.label
    out.mkdir(parents=True, exist_ok=True)
    tmp = dev / "data/local/tmp"
    drop = tmp / "omni-apk"
    drop.mkdir(parents=True, exist_ok=True)
    log = dev.with_suffix(".log")
    sh = lambda cmd, uid=None, t=300: run(dev, cmd, uid, t)

    # Not timed: nothing of the app on the device, the device idle again.
    print(f"[wj] device {dev.name}; uninstalling {PKG}", flush=True)
    sh(f"am force-stop {PKG}; pm uninstall {PKG}")
    for f in ("joining", "signed-in", "app-died"):
        (tmp / f).unlink(missing_ok=True)
    time.sleep(a.idle)
    store = drop / "Cookies"
    if a.mode == "preplant":
        make_store(store, Path(a.cookie))

    marks, first = [], {}
    pos = log.stat().st_size
    t0 = time.time()
    stop = threading.Event()

    def mark(name, extra=""):
        s = time.time() - t0
        marks.append((round(s, 2), name, extra))
        print(f"[wj] +{s:6.2f}s {name} {extra}".rstrip(), flush=True)

    def white(path):
        from PIL import Image
        px = Image.open(path).convert("RGB").resize((160, 90)).tobytes()
        n = len(px) // 3
        return sum(1 for i in range(0, len(px), 3) if px[i] > 235 and px[i + 1] > 235 and px[i + 2] > 235) / n

    def shots():
        last = 0
        while not stop.is_set():
            png = dev.with_suffix(".png")
            try:
                m = png.stat().st_mtime
                if m != last:
                    last = m
                    to = out / f"{int((time.time() - t0) * 1000):07d}.png"
                    shutil.copyfile(png, to)
                    # PS99's own loading screen (white, BIG Games) -- the end of the clock. Roblox's
                    # join screens before it are dark.
                    if "Joining game" in first and "in-game loading screen" not in first and is_loading_screen(to):
                        first["in-game loading screen"] = int(to.stem) / 1000
                        marks.append((int(to.stem) / 1000, "in-game loading screen", to.name))
                        print(f"[wj] +{int(to.stem) / 1000:6.2f}s in-game loading screen ({to.name})", flush=True)
            except OSError:
                pass
            stop.wait(a.shots)

    threading.Thread(target=shots, daemon=True).start()

    def tail():
        nonlocal pos
        with open(log, "rb") as f:
            f.seek(pos)
            data = f.read()
        cut = data.rfind(b"\n") + 1
        pos += cut
        for line in data[:cut].decode("utf-8", "replace").splitlines():
            for m in MARKS:
                if m in line and (m not in first or m in ("has died", "Joining game")):
                    if m == "has died" and PKG not in line:
                        continue
                    first.setdefault(m, time.time() - t0)
                    mark(m, line.strip()[:160])
                    if m == "DID_LOG_IN":
                        (tmp / "signed-in").write_text("1")
                    if m == "Joining game":
                        (tmp / "joining").write_text("1")
                    if m == "has died":
                        (tmp / "app-died").write_text("1")

    # Install (the APK handed over by a copy into the guest's /data/local/tmp).
    shutil.copyfile(a.apk, drop / "app.apk")
    mark("apk copied")
    act = "com.roblox.client/.startup.LauncherAliasMain"
    plant = ""
    if a.mode == "preplant":
        plant = (f"uid=$(stat -c %u /data/data/{PKG}); d=/data/data/{PKG}/app_webview/Default; "
                 f"mkdir -p $d; cp /data/local/tmp/omni-apk/Cookies $d/Cookies; "
                 f"chown -R $uid:$uid /data/data/{PKG}/app_webview; chmod 700 /data/data/{PKG}/app_webview $d; chmod 600 $d/Cookies; "
                 f"echo planted uid $uid; ")
    install = (f"pm install -r -d -g /data/local/tmp/omni-apk/app.apk; appops set {PKG} MANAGE_EXTERNAL_STORAGE allow; "
               f"rm -f /data/local/tmp/omni-apk/app.apk; ")
    if a.seed:
        # The app's own caches from an earlier run (no account data), copied in on the host after
        # the install, owned by the app in the same command that starts it.
        code, said, secs = sh(install, uid=0)
        mark("installed", said.strip()[:120])
        data = dev / f"data/data/{PKG}"
        n = 0
        for rel in SEED:
            src = Path(a.seed) / rel
            if src.is_dir():
                shutil.copytree(src, data / rel, dirs_exist_ok=True)
                n += 1
            elif src.is_file():
                (data / rel).parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(src, data / rel)
                n += 1
        mark("seeded", f"{n} of {len(SEED)}")
        plant += f"chown -R $uid:$uid /data/data/{PKG}/files /data/data/{PKG}/cache /data/data/{PKG}/shared_prefs; "
        install = ""
    code, said, secs = sh(
        f"{install}{plant}"
        f"act=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {PKG} | tail -1); echo launcher $act; "
        f"input keyevent KEYCODE_WAKEUP; am start -n $act{START_DATA.format(place=a.place) if a.link_in_start else ''}", uid=0)
    mark("installed+started", " | ".join(l for l in said.splitlines() if l.strip())[:300])
    if a.mode == "relaunch":
        cs = dev / f"data/data/{PKG}/app_webview/Default/Cookies"
        while not cs.exists():
            tail(); time.sleep(0.25)
        mark("cookie store made")
        end = time.time() + 10
        while time.time() < end:
            tail(); time.sleep(0.25)
        sh(f"am force-stop {PKG}")
        time.sleep(3)
        from subprocess import run as prun
        r = prun([sys.executable, str(Path(__file__).parent / "plant_cookie.py"), str(cs), a.cookie], capture_output=True, text=True)
        mark("planted", (r.stdout + r.stderr).strip())
        sh(f"am start -n {act}")
        mark("relaunched")

    link = (f"am start -a android.intent.action.VIEW -d \"roblox://experiences/start?placeId={a.place}\" "
            f"-n com.roblox.client/com.roblox.client.ActivityProtocolLaunch")
    deadline = t0 + a.limit
    sent, tries, loaded_at = 0.0, 0, None
    while time.time() < deadline:
        tail()
        now = time.time()
        if (tmp / "app-died").exists():
            (tmp / "app-died").unlink(missing_ok=True)
            sh(f"am start -n {act}")
            mark("restarted after death")
            sent = 0.0
        if a.link_at in first and "Joining game" not in first and (sent == 0.0 or now - sent > 90) and tries < 4:
            tries += 1
            sh(link)
            sent = time.time()
            mark(f"join intent sent (try {tries})")
        if "in-game loading screen" in first and loaded_at is None:
            loaded_at = now
        if loaded_at and now - loaded_at > a.after:
            break
        time.sleep(0.2)
    stop.set()
    json.dump({"label": a.label, "mode": a.mode, "marks": marks, "first": first}, open(out / "marks.json", "w"), indent=1)
    if not a.keep:
        sh(f"am force-stop {PKG}")
    print(f"[wj] done; shots in {out}")


if __name__ == "__main__":
    main()
