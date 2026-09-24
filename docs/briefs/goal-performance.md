# Brief: the performance goal -- fast, stable, lightweight on all three hosts

Written 2026-09-24 at `6d7f2b2` (`unified`). The `/goal` that points here is short; this is the
whole brief. Read it, then `docs/HANDOFF.md`, `docs/ports/{windows,macos,linux}.md`,
`docs/briefs/performance.md`, `docs/DECISIONS.md` and `crates/omnidroid/src/main.rs`.

## What "done" means

Measured **inside Pet Simulator 99 (place 8737899170) with the world loaded** -- menus, Home and
loading screens do not count:

| host | in-world target | reference |
|---|---|---|
| Windows | ~120 fps | the native Roblox app does 350+ fps on this machine; omnidroid does <10 |
| macOS | 60+ fps | |
| Linux | ~10+ fps | |

More is better; a little lower is acceptable. And on every host:

1. **Stable** -- joins and loads on the first launch of an account's storage and on a relaunch;
   30+ minutes with no guest thread dying, no crash, hang or freeze; a clean close
   (`SessionHistory "IAB"`/`"IB"`).
2. **Fast** -- the in-world targets above with smooth pacing (no multi-second hitches), and the
   shortest possible launch -> Home and join -> `onGameLoaded` (measure both).
3. **Lightweight** -- on the game's lowest graphics settings, as little RAM and CPU as possible;
   even on high settings never a sustained ~50% of total CPU or growth toward 10 GB. The standing
   requirement (HANDOFF): memory and CPU strictly on demand; boot peak <= ~4 GB settling near
   ~800 MB, so an 8 GB machine can run several instances.

Plus: the bottleneck list below worked through with before/after numbers for each, a screenshot
of each host in-world, all three checkouts at the same clean commit, docs updated.

## The three machines -- one repo, one branch (`unified`)

| host | checkout | notes |
|---|---|---|
| Windows (this PC) | `C:\Users\berat\Desktop\Omni Apps\omnidroid-unified` | Very powerful. Build with `OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-unified` (MAX_PATH). The other worktrees in `Omni Apps\` (`omnidroid` = perf-windows, `omnidroid-play`, ...) and the other `C:\od*`/`C:\odp*` build directories belong to other work: leave them alone. |
| macOS | `ssh berat@192.168.0.24`, `~/Desktop/omnidroid-unified` | Apple M1, 16 GB, ~12 GB disk free. Non-interactive ssh: `. ~/.cargo/env` first. |
| Linux | `ssh berat@192.168.0.38`, `~/Desktop/omnidroid-unified` | Ubuntu 26.04, i5-4460, **7 GB** (one cargo build or one app at a time), Quadro 4000 (Fermi: no Vulkan; GLES through nouveau `NVC0`). `cargo` is `/usr/bin/cargo`; there is no `~/.cargo/env`, and in `/bin/sh` a failed `.` kills the script. Never start an Xvfb that opens the nouveau render node (it wedged the GPU once and needed a reboot): `LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe Xvfb ...`. |

**Keeping them in step.** Every change is a commit on Windows, in the repo's commit style. Then
`git bundle create <f> <old>..unified`, `scp` it, and `git pull --ff-only <f> unified` on the Mac
and Linux. All three are at the same commit before any comparison between hosts. No stray files, no
unexplained uncommitted edits, no push to GitHub unless the owner says so. At `6d7f2b2` all three
had tree `ed6e877` and a clean status.

## Running the app

Each host's `Desktop/cookies` holds one account's cookie file (a different account per host).
From that folder:

```
<checkout>/target/release/omnidroid play --cookie <that .txt> --place 8737899170 [--minutes N]
```

Build first: `cargo build --release -p omnidroid` and
`cargo test -p omni-android --release --test gameactivity --no-run`. `play` runs the gate test
`initialize_native_code_returns_a_native_code_and_the_game_thread_starts` as the session; the
gate failing because a guest thread died is a real defect, not noise. Each account's storage is
`<app-data>/../accounts/<name>`.

* **Linux:** `export DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.*)` and
  start with `setsid nohup ... &` -- a plain `nohup ... &` dies with the ssh session.
* **Windows:** start it detached (`Start-Process cmd /c ...`) so no tool timeout kills it.
* **End every run cleanly:** the window's X (Windows: `CloseMainWindow()`) or `--minutes N`. A
  killed run is judged a crash at the next launch of that storage; after one, delete that
  account's folder and start fresh.
* **Live tests on one host at a time.** Building, and subagents writing code, may run in parallel
  on all three; running the app may not. Never run one account on two hosts at once.

**Network and accounts.** Roblox is blocked by the ISP; each host uses a bypass (WARP /
SplitWire). The accounts are Turkish, and **Roblox ends a session within a second when its cookie
is used through an exit in another country** (DID_LOG_IN, then 401 on every call, then
DID_LOG_OUT; measured three times on 2026-09-24 -- the app's own logout calls come after the
401s). Before every run: `curl https://www.cloudflare.com/cdn-cgi/trace` shows `loc=TR`, and
`https://users.roblox.com/v1/users/authenticated` with the cookie answers 200 (print the status
and name only). Never print, log or copy cookie values or passwords; never work around Roblox
security (captchas, the country check); if a cookie dies, stop and ask the owner for a new one.

## Log markers (the run's stderr)

| marker | meaning |
|---|---|
| `DID_LOG_IN ... "username":"..."` | signed in |
| `JOIN: nativeAppBridgeV2StartGameWithParam(place=8737899170) returned 1` | the join was requested |
| `UgcExperienceController: submitStartGameTask`, `! Joining game '...' place 8737899170 at <ip>`, `Connection accepted`, `Replicator created` | the join is running |
| `onGameLoaded: placeId:8737899170` | the world has loaded; in-world measurement starts after this plus a settle period |
| `FRAMES: +Ns into the session, T presents (+X in the last 5s)` | fps = X / 5 |
| `GUEST THREAD DIED ...` | always a bug |

`OMNI_PERF=5` adds per-thread PERF blocks (jit / monitor / dispatch / handler shares, crossings,
translations). Other switches (HANDOFF): `OMNI_WAIT_TRACE`, `OMNI_JIT_CACHE_MB`,
`OMNI_JIT_EXCLUSIVE_MONITOR`, `OMNI_IMPORT_CENSUS`.

## Known blockers -- fix these first

1. **The join dies silently (6 of 8 runs on 2026-09-24).** About 5 s after the join call, the
   worker started at link `0x22d457c` calls `pthread_cond_timedwait` with a ~120 s deadline.
   `bionic/handlers.rs` refuses any wait over `MAX_SLEEP_SECONDS` (60) and **kills the thread**:
   the join never starts, the loading screen stays forever, rendering carries on. Every run in
   which it survived (two warm relaunches on Windows) joined and loaded; every run in which it
   died never logged `submitStartGameTask`. Fix: wait in slices of at most 60 s, checking for a
   stop request between them, and answer ETIMEDOUT only at the real deadline (the idea of
   `wait_a_slice` in `bionic/net.rs`) -- that keeps what the cap protects and the rejected-clamp
   reasoning. Then look for the same refusal in `nanosleep`, `poll`, `select`, `ALooper_pollOnce`.
2. **`remove` is not implemented** (the guest calls it through a thunk). On Linux it killed a
   thread and hung a session on the second launch of an account's storage (kept as evidence:
   `~/.local/share/omnidroid/accounts/*.hung-remove-0924`).
3. **The InferredCrash reporter** kills a worker at +5 s after an earlier crash (HANDOFF).

Fix anything else that fails a run or kills a thread, with a test where the repo's conventions
allow one.

## Bottlenecks -- find them all, then solve them one by one

From in-world profiles (`OMNI_PERF=5`, idle camera 2-3 minutes, then moving), list **every**
bottleneck: where frame time, CPU and memory go on each host, ranked by measured cost -- JIT
translation and invalidation, guest/host crossings and dispatch, the exclusive monitor, handler
cost, waits and synchronization, the GPU/presentation path, frame pacing and throttles, memory
commit and growth. Keep the list in `docs/HANDOFF.md`, each item with its measured share and host.

Then, most expensive first: measure -> fix -> re-measure in the same scene -> commit -> update the
list -> re-profile (fixing one exposes the next). Keep going until the targets are met or the list
is empty, and write down why anything left cannot be improved. n >= 2 per arm; report median and
spread; record every number in `docs/ports/<os>.md` and HANDOFF with the commit it was taken at.

Before optimizing, check what `perf-windows` already did (208c4c8 invalidations, bae4b92 monitor,
dynarmic patches 0002/0003) and whether it is in `unified` (`git log unified..perf-windows`). When
vendored dynarmic C++ changes, touch `crates/dynarmic-sys/vendor/PIN.txt` before building. Never
buy speed with correctness (no skipped engine work, no fake frames); a trade-off is written in
`docs/DECISIONS.md`.

## Subagents

The owner's time matters more than tokens: use subagents wherever they save wall-clock time
without lowering quality -- in parallel for decoding engine code, independent fixes, builds on the
Mac and Linux while Windows measures, audits of a fix, docs. The main agent keeps the ordering and
the final judgement of every measurement, reviews each result before trusting it, and holds
subagents to the same rules (one host runs the app at a time, the repo stays clean and in step).

## Screenshots (proof: each host in-world at each milestone, title bar visible)

* **Windows:** `System.Drawing` `CopyFromScreen` of the monitor the window is on
  (`Screen.FromHandle`).
* **macOS:** `screencapture` is refused over ssh. Write a `.command` that calls it and run it
  with `open -g -a Terminal`. **Never run `osascript` over ssh** -- it raises an Automation
  permission dialog on the owner's screen.
* **Linux (Wayland):** only the app's own X window can be read -- `xwd -id <the client child of
  the Mutter frame>`; the frame and the root answer `BadMatch`.

Ask the owner before anything destructive or outside the repo, and for a new cookie or a VPN
change.
