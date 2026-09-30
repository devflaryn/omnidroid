# One shared Android, many sandboxed app instances (Spec A)

Today every `start_instance` boots a whole Android of its own -- system_server, ~68 guest daemons,
the HALs and a window in one system host process (~1.7 GB private), ~450 MiB of small helper app
hosts, and the app's host process (Roblox: ~2.7 GB commit). This spec makes the system layer boot
once and be shared: each app instance is an **Android user** of a shared system, shown on a
**display of its own**, in its own app host processes. Instance N+1 then costs its app host and
its user's helpers, not a fresh Android.

Multi-instance is not a mode: it is always available, launching never stops another instance, and
one instance behaves -- and costs -- exactly what it does today.

Spec B (next): the live-attach debugger and the MCP instance selector, built on this spec's
instance registry. Nothing here depends on it.

## Requirements (from the mission, as agreed)

1. **One shared system**: system_server, zygote, HALs, composer and property service boot once per
   system; instances attach to it. Persistent.
2. **`omnidroid warm`** boots a system only (no app, no window) and is idempotent.
3. **Two launch paths**, both always available: after a `warm` (fast attach), or without one (the
   launch warms lazily, and later launches reuse that system).
4. **Mutual invisibility**: an instance cannot observe another -- packages, processes, `/proc`,
   `/data`, binder, files, sockets, **input or pixels**.
5. **Same package, different versions, in parallel** (e.g. stock Roblox and a Delta build).
6. (Spec B) live debug tools take an instance selector.
7. **No boot animation and no "starting" text**, ever.
8. **No window or presenter until the instance's app is ready to draw**; `warm` opens no window.
9. **No regression**: the single-instance path, its fps and RAM, the perf levers, the MCP lab
   tools, and Windows x64 / Linux x64 / macOS arm64.

## Decisions

1. **An instance is an Android user.** Android 15's *visible background users* put each user on a
   display of its own; the real framework then scopes packages, processes, tasks, providers,
   accounts, clipboard, notifications, per-user settings and `/sdcard` to the user. An instance's
   id inside a system is its user id, `uid / 100000`. This keeps the rule of the binder design
   (decision 5): the real system_server, never a framework service replaced by a written answer.
2. **Requirement 5 is met by a pool of shared systems, not inside one.** PackageManager keeps one
   package record -- one code path -- per package name for the whole system, across users, and a
   different signer is refused even for another user. Two versions of one package therefore
   cannot share a system without faking package identity in binder replies (rejected: brittle, and
   against decision 1). A launch whose package conflicts with one already installed in every
   running system boots (lazily) another shared system, which later launches of that version
   reuse.
3. **Slot 0 is user 0 on display 0**, exactly today's configuration: one instance costs what it
   costs today. Further instances are users 10, 11, ... on displays 1, 2, ...
4. **A supervisor process owns policy; the system host is mechanism.** The supervisor
   (`omnidroid supervise`) holds the registry, the pool, the user slots and every orchestration
   step; a system host (`omni-linux-run`) exposes a small authenticated control endpoint and knows
   nothing of instances.
5. **The instance boundary on the host is enforced below the framework** where the framework
   cannot see: binder identity, the guest's view of `/data`, abstract sockets, loopback ports,
   input devices and composed pixels.

## Spike: visible background users in this image (2026-09-30)

Three boots of the aosp-35 image with `fw.visible_bg_users=1`, `fw.max_users=8`,
`fw.show_multiuserui=1` and an overlay display (`overlay_display_devices`, Android's own simulated
display -- no composer change):

- `UserVisibilityMediator`: "Supports visible background users on displays: true", max users 8.
  No framework patch: the properties are read by `framework.jar`.
- `pm create-user` gave user 10; `am start-user -w --display 2 10`: "Success: user started on
  display 2" (~1.4 s); visible users `[0, 10]`.
- User 10's Settings drew on display 2 (screenshot); user 10's Roblox (`u10a115`) made its Vulkan
  device and swapchain and was display 2's resumed activity at +90 s while display 0 kept user 0's
  own resumed activity. It was still on its loading screen at +90 s (not compared to user 0).
- `pm list packages -3 --user 0` was empty while user 10 listed `com.roblox.client`.

What it taught (each is in the design below):

- **User 10 starts its own helper app hosts**: ext.services, permissioncontroller, MediaProvider,
  devicelockcontroller, keychain -- per-instance RAM, the target of the per-user lean step.
- **`omni_autogrant.sh` grants for `--user 0` only**: user 10's Roblox, not granted
  `MANAGE_EXTERNAL_STORAGE`, opened the "All files access" page, was paused, and was killed.
- **A display's apps die when its display group sleeps**: the 60 s screen timeout put display
  group 0 (which the overlay display shares) to sleep, and the lean kiosk kills stopped apps at
  once (`cached #1`).
- **Backgrounding is fatal** in the lean kiosk: nothing may ever cover or stop an instance's app.
- **A secondary display letterboxes** an app that asks for an orientation (405x720 on 1280x720).
- Not proven by the spike: input routing to a secondary display (an overlay display has no input
  port) and Roblox reaching in-world on one. Both are gates below.

## Architecture

```
 omnidroid warm|aosp|ps|stop      omni-mcp
            \                       /
             \  token, framed JSON /
              v                   v
        +-----------------------------+      <app-data>/supervisor.json (0600 / owner DACL)
        | supervisor (omnidroid       |      <app-data>/supervisor.lock (held for life)
        |  supervise): registry, pool,|
        |  slots, recipes, admission  |
        +-----------------------------+
          | stdin (token; EOF = exit)  | control endpoint (token)
          v                            v
   +-------------------------------+   +-------------------------------+
   | system host sys-1             |   | system host sys-2  (pool)     |
   | omni-linux-run: init, system_ |   | ...                           |
   | server, daemons, gralloc,     |   +-------------------------------+
   | composer (displays 0..k),     |
   | windows, zygote stand-in,     |
   | binder listener, control      |
   +-------------------------------+
      |  binder (credential)   |
      v                        v
   app hosts of user 0      app hosts of user 10 ...
```

### Components

- **`crates/omni-supervisor`** (new): the control protocol, the client used by `omnidroid` and
  omni-mcp, and the supervisor itself (registry, pool, slots, recipes, admission, crash handling).
  `omni-mcp`'s `json.rs` moves here and omni-mcp uses it from here: no new dependency.
- **`omnidroid`**: subcommands `supervise` (the daemon), `warm`, `aosp` (now a client), `ps`,
  `stop`. `aosp` keeps `--apk --cookie --place --minutes --size --gpu --with-systemui` and gains
  `--detach`.
- **`omni-linux`**: the boot recipe moves from `tests/common/boot.rs` into the library
  (`omni_linux::boot::SystemBoot`: `derive_classpath`, init classes, HALs, zygote, system_server
  argv). `tests/common/boot.rs` and `r_roblox` call it: **one boot path**. New modules: the control
  endpoint (`control.rs`), multi-display composer, per-user namespaces.
- **omni-mcp**: `start_instance` / `stop_instance` / `list_instances` / `screenshot` go through the
  supervisor; it holds the `LAUNCH` connections of the instances it started. Lab tools unchanged.

### Supervisor singleton

`<app-data>` is `omni_platform::process::app_data_dir()`.

1. A client takes `<app-data>/spawn.lock` (exclusive, short-lived, blocking with a timeout).
2. It tries `<app-data>/supervisor.lock` without blocking. A running supervisor holds that lock
   for its whole life, so:
   - **lock acquired** -> no supervisor is alive: release it, spawn `omnidroid supervise`
     (detached), wait until it answers a probe;
   - **lock busy** -> a supervisor process is alive: probe it (connect to the port in
     `supervisor.json`, `HELLO{token}`, 2 s timeout, 3 tries). Answers -> use it.
     No answer -> **takeover**: verify the pid in `supervisor.json` is ours (image name and the
     process start time stored in the file -- a reused pid is never killed), kill it, confirm
     `supervisor.lock` is now free, spawn.
3. Release `spawn.lock` only after the new supervisor answers a probe: two clients starting
   together cannot both spawn.

`supervisor.json` = `{pid, start_time, port, token, build}`; created 0600 on Unix, with a DACL
granting only the current user on Windows. `HELLO_OK` carries the supervisor's build id; a client
of another build refuses ("run `omnidroid stop --all`") and never kills running instances itself.

### Protocol

Framed as `remote.rs` frames are (u32 length, u8 kind, payload); control payloads are JSON
(`omni_supervisor::json`). Every connection begins with `HELLO{token, build}`.

Client -> supervisor:

| Request | Answer / events |
|---|---|
| `WARM{count}` | `WARMED{systems}` once at least `count` systems are booted |
| `LAUNCH{apk, cookie?, place?, size, gpu, flavor, detach}` | `PROGRESS{step}`..., then `READY{id, system, user, display, pids}` or `FAILED{step, detail}`; later `INSTANCE_DEAD{reason}` |
| `PS` | every system and instance (below) |
| `STOP{inst-N \| sys-N \| all}` | `STOPPED` |

An attached instance lives exactly as long as its `LAUNCH` connection: Ctrl-C, `--minutes`
elapsing, or omni-mcp dropping it tears that instance down. With `detach` the client returns at
`READY`, printing the id, and the instance lives until `STOP` or its system's end.

Supervisor -> system host control endpoint (127.0.0.1:0, announced on the host's stdout as
`[control] <port>`; the per-system token arrives on stdin, never argv; every frame carries it):

| Op | Does |
|---|---|
| `EXEC{argv, uid=shell}` | runs a guest command, returns exit code and output |
| `DISPLAY_ADD{w, h, dpi}` / `DISPLAY_REMOVE{k}` | composer hotplug; returns HWC display and port |
| `WINDOW_WHEN_READY{k, package, title}` | open display k's window on its first frame with that package's layer |
| `PIDS` | app hosts by pid, uid and process name (the zygote's children) |
| `KILL_USER{u}` | ends every app host of user u |

Events: `BOOT_COMPLETED`, `APP_HOST_STARTED/ENDED{pid, uid, name}`, `FIRST_APP_FRAME{k}`.

### Lifecycle and failure

- **System persistence**: systems stay booted with zero instances. Only `stop sys-N|all` ends
  one; `stop all` also ends the supervisor. `warm --count N` keeps a floor of N booted systems.
- **No orphans**: a system host exits when its stdin (a pipe from the supervisor) reaches EOF; an
  app host exits when its binder connection to the system is lost (new; verified by test). On
  Windows each system host and its app hosts are in a Job object with kill-on-close held by the
  supervisor; on Linux/macOS, a process group.
- **Unexpected system exit** (seen: system_server killing itself on Linux): the supervisor watches
  each system host's process and control connection. The system is marked `crashed` with its exit
  code and last log lines; each of its instances becomes `dead(system crashed)`; attached clients
  get `INSTANCE_DEAD` (`omnidroid aosp` exits non-zero), detached ones show dead in `ps`. Instances
  are **never relaunched** (their login and place are gone); the system is re-booted only to hold
  the warm floor, with backoff, and after 3 crashes in 10 minutes the supervisor stops trying and
  reports it.
- **Logs**: the supervisor drains each system host's stdout/stderr into `<sys>/system.log` (a full
  pipe never stalls a system). The zygote pipes each app host's output to `<sys>/logs/<pid>.log`
  and into `system.log` prefixed `[pid]`; the supervisor maps pid -> uid -> instance, so an
  instance's markers (sign-in, joining) are read from its own processes only.
- **An app host dying** is Android's business; the instance becomes `app-exited`, its client is
  told, and its slot is torn down.
- **RAM admission**: before booting a system (needs `OMNI_ADMIT_SYSTEM_MB`, default 4096) or adding
  a slot (`OMNI_ADMIT_SLOT_MB`, default 3072), the supervisor reads available memory (Windows:
  available physical and commit headroom; Linux: `MemAvailable`; macOS: free + inactive pages) and
  answers `FAILED{admission, needed, available}` rather than letting the guest's low-memory killer
  take neighbours down. Refused, not queued; the caller retries.

### Registry

`sys-N`: `{state: booting|ready|crashed|stopping, dir, pid, flavor, claims: {package -> (apk
sha256, users)}, slots, displays}`. `inst-N` (numbered for the supervisor's life):
`{system, user, display, package, versionName, versionCode, apk sha256, pids, state:
launching|running|app-exited|dead|failed, attached|detached, window, screenshot path}`. `ps` and
MCP's `list_instances` print it.

## Launch

**Pool key** = (package, SHA-256 of the APK file). A second user of a system can only share the
installed code, so byte identity is exactly the test; no signature parsing. `omni-apk` supplies
package, versionCode and versionName for display.

**Flavor** = what is fixed per system: image apps (`kiosk` or full, from `--with-systemui`),
locale (`persist.sys.locale`), GPU (`--gpu`). A launch only joins a system of its flavor.

**A system is unclaimed until a launch installs a third-party package in it**; that launch claims
it for (package -> key). A claim is released when no user of the system still has the package (the
package is then removed entirely, so the next claim installs any version clean). A different
version launched after one `warm` therefore cold-boots a second system (unless `warm --count 2`).

`LAUNCH`:

1. **Admission** (above).
2. **Pick a system**, under the registry lock, among the systems of the launch's flavor, in this
   order: one with the same key installed; the
   busiest where the package **name** is absent; an unclaimed booted one; one still **booting**
   (the launch waits for it -- two concurrent lazy launches share one boot -- and re-validates its
   choice under the registry lock once it is up); else cold-boot a new one. A system is full at
   its slot cap (`OMNI_SLOTS`, default 16; `fw.max_users` = cap + 1).
3. **Slot**: the lowest free. Slot 0 is user 0 on display 0. Slot k: `pm create-user`,
   `DISPLAY_ADD`, wait for DisplayManager to report the display, `am start-user -w --display <d>
   <u>`.
4. **Install**: first install of the package in the system: `pm install -r -g --user u`. Further
   users: `pm install-existing --user u` -- **never** `install -r`, which kills the package's
   processes in every user. Then **per-user grants**: the runtime permissions it requests and the
   special app-ops (as `omni_autogrant.sh` does, for user u), **never** a development or
   system-wide permission that crosses the instance boundary (`READ_LOGS`, `DUMP`, and the like).
5. **Per-user setup**: `user_setup_complete`, lock screen disabled, `screen_off_timeout` at its
   maximum, and the **per-user lean step** (`pm disable-user --user u` of the helper apps the
   instance does not need; the list is established empirically in phase 3). Global, once per
   system: provisioned, stay awake, animation scales, force-resizable, the image's `IDLE_APPS`.
   Per display: `wm set-ignore-orientation-request -d <d> true`.
6. **App recipe** (the Roblox recipe, beside the generic launch): wait for the app's cookie store at
   the host path of user u's data (`<sys>/data/user/<u>/<pkg>/app_webview/Default/Cookies`; for
   user 0 `<sys>/data/data/<pkg>/...`, which vold binds as `/data/user/0`), `am force-stop --user
   u`, `tools/plant_cookie.py`, `am start --user u --display <d> -W`; after the instance's own log
   shows sign-in, the place's deep link with `--user u --display <d>`, retried as `r_roblox` does.
7. **Ready**: `FIRST_APP_FRAME{d}` -> `running`; the window opens (below); `READY` to the client.

Every step has a timeout; a failed step leaves the instance `failed(step, output)`, its slot freed.
`pm`/`am` mutations are serialized per system; systems work in parallel; boot waits hold no lock.

**Teardown** (connection drop or `STOP`): `am force-stop --user u <pkg>`, `KILL_USER(u)`; slot 0:
`pm uninstall --user 0 <pkg>` (its data goes with it); slot k: `am stop-user -w -f u`,
`pm remove-user u`, `DISPLAY_REMOVE`. The window closes; the claim is released if no user still
has the package.

The single-instance path is this path: `omnidroid aosp` = lazy warm + slot 0. `omnidroid aosp` no
longer runs `cargo test`; it needs `omni-linux-run` built beside `omnidroid` (`cargo build
--release -p omnidroid -p omni-linux`). Its start-up wipe of every `omni-shm-*` goes: it would take
a running neighbour's buffers. Each system removes its own leftovers (its dir, and on Linux its
`/dev/shm` entries) when it stops, and the supervisor sweeps those of crashed systems.

## Isolation

The framework scopes most of it: an app cannot hold `INTERACT_ACROSS_USERS`, so its package and
process lists, tasks, provider resolution, accounts, clipboard, notifications, per-user settings
and `/sdcard` (`/data/media/<u>`) are its user's. The host enforces the rest:

1. **Binder identity.** The zygote issues each app host a credential at launch (on its stdin); the
   binder listener binds the stand-in's pid and uid to the credential and ignores what an `OPEN`
   frame claims. Today any local process -- or a guest socket -- can claim uid 1000 or another
   instance's uid.
2. **Loopback is virtual, per (system, user).** Guest TCP/UDP sockets are the host's (`inet.rs`,
   `hostnet.rs`): a guest bind of `127.0.0.1:P` is a machine-wide host port today, so instances
   collide across systems, and any guest can reach omnidroid's own listeners (binder, xsocket
   brokers, relays, control endpoints) and the owner's other local services. Now a guest bind to
   a loopback address binds the host's `127.0.0.1:0` and records the guest's port in
   `<sys>/.omni-loopback/<ns>/<proto>-<port>`; a guest connect to a loopback address resolves only
   through its own namespace, and an unknown port is `ECONNREFUSED` -- a real host port cannot be
   named at all. App uids (appId >= 10000) have their user's namespace; system uids share one per
   system. Non-loopback traffic is unchanged. Omnidroid's own plumbing (relays, xsocket brokers,
   the binder listener, the control endpoints, the cookie plant) is host code on `std::net` or
   files, not guest sockets, and is unaffected.
3. **Abstract unix sockets** are per host process and published under `<sys>/.omni-sockets/`
   (`xsocket.rs`): separate systems never see each other's names. Within a system, app uids get a
   per-user namespace (`.omni-sockets/u<u>/`); system uids keep the shared one.
4. **The guest's `/data`** in an app host of user u: only u under `/data/{user, user_de, misc_ce,
   misc_de, system_ce, system_de, media}/`; `/data/data` only when u is 0; `/data/local/tmp`
   hidden from app uids; under `/data/app/` only the app's own package's directory. Anything else
   is ENOENT and absent from directory listings -- invisible, not merely denied.
5. **`/proc`** already shows a process its own pid only; kept, and tested across users.
6. **Input**: each display's window has input devices of its own, associated with that display
   only (below): a touch or key on instance A's window can never reach instance B.
7. **Pixels**: display k's composed frame -- and so its window and screenshots -- holds only
   display k's layers, which are user k's. Buffers are host shm files the guest cannot name.
8. **Properties**: apps cannot set them, and no per-instance datum is ever written to one.

Not covered: servicemanager's list (the same system services for every instance; apps' own binders
are not registered there) and timing or resource side channels.

## Displays, windows, input

- **Composer displays.** `hal/composer.rs` goes from one display to a map of displays, each with
  its `Framebuffer`. `DISPLAY_ADD` makes physical HWC display k with port k and sends SurfaceFlinger
  a hotplug CONNECTED; `DISPLAY_REMOVE`, DISCONNECTED. SurfaceFlinger composes per layer stack;
  the composer presents each display's frame into that display's framebuffer only.
- **Never asleep.** If the health HAL reports AC power, `stay_on_while_plugged_in` plus the
  per-user maximum `screen_off_timeout` keeps every display on; otherwise each display is given a
  display group of its own. The S4 spike measures which.
- **Input routing.** Each display's window registers its own evdev set (touch, keyboard, mouse)
  with `phys` = `omni-display-<port>`. The image carries `input-port-associations` for every port
  0..cap, written at image build, so a hotplugged display's devices are mapped the moment it
  appears. InputReader stamps each event with its display. Keys need a focused window on every
  display at once: `config_perDisplayFocusEnabled`, set by a **static RRO in the device image**
  (deterministic; a fabricated overlay is the fallback only if the RRO cannot be built).
- **Windows open on the first app frame, display 0 included.** A display's window and presenter
  thread are created on `WINDOW_WHEN_READY{k, package}`, at the first composed frame of display k
  containing a layer of that package -- after boot_completed and the app's first surface. Today
  display 0's window opens when the composer is registered; that goes. So `warm`, the boot,
  FallbackHome, an empty display mirroring display 0 and any "starting" text are never on screen:
  there is no window until the app draws. Boot animation is not run at all
  (`debug.sf.nobootanimation=1`, and `bootanimation` in `LEAVES_OUT`: less boot time and RAM).
  Titles: `omnidroid — inst-3 · com.roblox.client 2.738.1397`. Closing a window leaves its
  instance running headless, as today; `stop` ends it. Screenshots are per display:
  `<sys>/shots/display-<k>.png`, reported by `ps`.
- **Cost per display.** Compose and present run once per display per frame. The compose fast path
  (`compose_fast`, a live lever, default off) is A/B'd with several displays in phase 2 under the
  perf-night rules (>= 8 interleaved pairs, side branch, default flipped only on evidence).

## Phases and gates

Every phase is reviewable, green on all three hosts, and keeps `r_roblox` green. Gates use the
probe APK (`tests/fixtures/probe-app`, extended; built at versionCode 1 and 2 with one keystore)
wherever Roblox is not the subject.

| Phase | Delivers | Gates |
|---|---|---|
| **0: guest network boundary** | virtual loopback per (system, user); binder credential | a guest cannot reach a real host listener; two namespaces cannot reach each other's ports; relays, xsocket, binder unaffected; `r_roblox` green |
| **1: runtime** | library boot (`r_roblox` uses it); supervisor, singleton, takeover, tokens, file modes; control endpoint; `warm` / `aosp` / `ps` / `stop` / `--detach`; lazy warm; crash handling; admission; MCP start/stop/list/screenshot via the supervisor; **slot 0 only**; window on first app frame and no boot animation | unit: spawn race, takeover (incl. a reused pid), token refusal, pool choice; `warm_then_attach` (no window while warm); `lazy_warm` (second launch reuses the system; neither stops the other); system-crash; admission refusal; **regression floor**: one instance through the supervisor = the direct boot of the build the phase started from (`main` before phase 1), in private commit and in-world fps, within the noise band the pairs themselves measure (interleaved across the two builds, >= 8 pairs, same session) |
| **2: displays** | prerequisite: the minimal user-on-display primitive (create-user, `DISPLAY_ADD`, start-user `--display`) as the S4 spike uses it; composer multi-display; input association and RRO; power; letterbox fix; `compose_fast` A/B | S4 spike then `d9_multi_display`: **visual isolation** (each user's app paints its own solid colour; display k's framebuffer holds only user k's colour, pixel-checked over a window of frames), **input non-leakage** (touch and keys on window k reach only user k; user 0's app on display 0 receives nothing), keys with focus per display, never asleep, no letterbox |
| **3: users as slots** | full slot lifecycle: install-existing, per-user grants and their guard, per-user lean, teardown; per-user `/data` view; per-user abstract sockets | `g_isolation` with the **adversarial probe** (two users, same key): each lists only itself, and each actively tries and fails to reach the other -- opens its `/data/user/<u>/<pkg>` by absolute path, connects to its known loopback port and abstract name, stats and signals its pid; input leak case. **Scale gate**: Roblox reaches in-world on display 1 beside a user-0 instance in one session; join time and fps logged for both; user 0's fps with instance 2 running does not drop beyond the noise band. If it does, the cause is found: the shared system (system_server, composer) is a defect to fix; raw host saturation is recorded and sets admission |
| **4: pool** | key matching, claims and their release, a second system for a conflicting version | `g_isolation` across systems; `two_versions`: probe v1 and v2 in parallel, each reporting its own versionCode |
| **5: measurement** | `tools/multi_ram.ps1` and `.sh` | report in `docs/`: private commit of 1, 3 and 6 rendered Roblox instances by role (supervisor, system host, each instance's app host, per-user helpers), per-user lean before/after, fps per instance; base + X per extra instance vs ~3 GB x N today |

No work on more than two slots starts before phase 3's scale gate passes.

**The measurement's claim.** Phase 5 measures the **rendered** ceiling and says so: 25-30
instances on 32 GB needs GPU-drop headless (a separate roadmap, `headless-notebooks-goal`) and is
not claimed here. One instance with its window closed is reported as an indicative data point, not
as a projection.

**Cross-host.** After each phase Windows syncs to Linux and macOS over SSH and the suites run
there; Linux cleans its per-system `/tmp` and `/dev/shm` leftovers. macOS runs with `ulimit -n
65536`, and its display, window and GPU gates run locally in its GUI session, out of band: the
per-phase SSH sweep covers macOS's non-GPU logic only.

## Risks

- **Per-display focus and input association** are proven only by the S4 spike; if the RRO route
  fails and the fabricated overlay is too late at boot, keys need another route (the design then
  returns here).
- **Roblox on a secondary display** reached its loading screen, not in-world, in the spike: the
  phase 3 scale gate exists for this.
- **Per-user helpers** may not all be disableable without breaking the app (MediaProvider backs
  `/sdcard`); what cannot be disabled is per-instance RAM and is reported.
- **Contention**: system_server and the composer are shared; N instances multiply their load.
  Phase 3's fps gate and phase 5 measure it.

## Out of scope

The live debugger and MCP instance selectors (Spec B); GPU-drop headless; instances surviving a
system restart; timing and resource side channels between instances; a real zygote fork.
