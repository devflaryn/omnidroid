# Brief: why a Roblox game world runs at 2-8 frames per 5 seconds, and fixing it

Rewritten 2026-09-23 night, after the first game world loaded and rendered. The first version of
this brief (menus only) is in git history; its measurements are kept below because they still
hold for the menus. **This version measures in the game world.** The owner's verdict on the world:
"way too slow -- I can barely move my camera. I want it smooth."

**Hard constraint: the APK is arm64-v8a only.** There is no x86-64 build to switch to (the one Sober
uses is not available here). All performance work is ARM64 guest code through dynarmic.

Read `docs/HANDOFF.md` from "# START HERE" (especially "2026-09-23 night: A GAME WORLD LOADS AND
RENDERS") and `docs/VERIFICATION.md` first: how measurements here have gone wrong before. Trust
runs over summaries, including this one.

## What is measured in the world (play17, 2026-09-23, the owner's session)

The FRAMES line (`+Ns into the session, P presents (+d in the last 5s)`), one run, n = 1 session:

| phase | presents per 5 s |
|---|---|
| menus, settled / animating (before Play and after the kick) | 5 (1/s idle) / 290-300 (~59 fps) |
| Play pressed -> lobby connected (+140 s) -> lobby loaded (+197 s) | 0-5 |
| teleport (+218 s) -> main world connected (+253 s) -> loaded (+311 s) | mostly **0** for ~60 s |
| **in the main world** (+335 s to +914 s), owner moving the camera | **1-15**, bursts to 40-87 |

* The lobby and the world each took about a minute of zero frames to load, while the Lua main
  thread ran heavy module loads. **The game times its own loads** and prints the slow ones -- the
  same modules on every join, so they are a repeatable, engine-supplied measure of guest execution
  speed: play17, lobby then world, `[SlowBenchmark] Functions` 930 / 1068 ms, `Types` 4883 / 5335,
  `Items` 5172 / 5820, `GUILoader` 2629 / 2766, `[SlowModule] PetItem` 3322 / 3669, `MachineCmds`
  757 / 839 (`grep -a "SlowBenchmark\|SlowModule"`). What a phone takes for them is not known here;
  do not invent a figure.
* At +914 s the server disconnected the client: reason 304, "Roblox has detected missing or
  corrupted files". Not investigated (not yours; see the handoff). **An in-world run lasts about
  ten minutes of connection at most** -- plan measurements that fit.
* Memory: at +991 s (after the kick, back at the menus) the engine reported **6.4 GB in use**
  and unloaded its Lua app on a low-memory warning; the gate's guest commit ceiling is 8 GiB. Keep
  an eye on it -- this host's free commit is small (see Constraints).
* A loaded world runs more than 64 guest threads at once (bionic's table was raised to 256 for
  it; how many exactly was not recorded).

## What is measured on the menus (the first version, still valid there)

- CPU is not saturated on the menus: 2.3-2.4 cores. One host thread at ~97% is guest thread 5,
  the GameActivity game loop spinning on `ALooper_pollOnce(0)` + a mutex (DECODED at `0x2bcd648`;
  `DoFrame` `0x2bd1cf0` does nothing in the steady state). 2.5-3.6 M crossings/s, ~30%
  `ALooper_pollOnce`, ~45% mutex, ~25% guest code.
- The render thread (guest thread 6 on the menus) spends 81% in `pthread_cond_wait`; workers
  60-99% in `syscall` (futex).
- `fdb2f88`: TaskScheduler workers were re-translating ~290,000 guest instructions/s because
  dynarmic evacuates a thread's whole cache when under 1 MiB is free; a 32 MiB cache per guest
  thread took menu dragging from 13 to 60 fps (`CODE_CACHE_BYTES`, `OMNI_JIT_CACHE_MB`).
- Cold translation on real code is **0.516 M guest insn/s** (n=11), warm execution **156.7** (n=31)
  -- the handoff's "Measured figures". A world load runs a great deal of code for the first time,
  on many threads, **each with its own code cache** (dynarmic's `Jit` is per thread here): the
  same hot function is translated once per thread that runs it. Whether that is where the load's
  minute goes is the first hypothesis to test, not a finding.
- The 1 ms timer (`1634316`, `omni_platform::clock::TimerResolution`) is held for the session; its
  effect is NOT measured.

## Instruments that exist, and what they cannot see

- `OMNI_PROFILE=1` (`sample_profile` in `tests/gameactivity.rs`): per guest thread, share of
  samples in guest code versus each handler, crossings/s. **Prints once, at session end, for the
  whole session** -- menus, load and world mixed. **"In guest code" includes dynarmic translation**
  (it happens inside `Jit::Run`), so it cannot separate translating from executing.
- `OMNI_WAIT_TRACE=<s>` (`omni_android::waits`): every handler timed by guest thread, call site and
  object from second `<s>`; totals at session end. Same end-of-session limitation.
- `omni_cpu::dynarmic::count_code_fetches` / `code_fetches_by_thread` (`ce8a2d8`): guest
  instructions fetched for translation, overall and per host thread. Printed on the FRAMES line
  while the wait trace is on.
- The FRAMES line (every 5 s) and `GUEST THREAD DIED` lines. `OMNI_LATE_TAP`, `OMNI_LATE_INPUT`
  for synthetic input. Other switches at the top of `tests/gameactivity.rs` and in the handoff.

## How to get into a game world without a person

A world needs a signed-in account; signing in needs the owner (Quick Sign-in, on their phone).
**The controller will give you a signed-in data directory that was closed cleanly** (the path is in
the message that launches you, or will be sent to you). Rules for it:

* **Never run on it in place.** Copy it to a fresh directory for every run (`OMNI_DATA_DIR=<copy>`),
  and end every run cleanly (the gate's own session end, or closing the window) -- a run killed any
  other way leaves a directory whose next launch dies in the engine's inferred-crash report.
* One run at a time on this account: a second join by the same user kicks the first.
* Getting from Home into the world: the owner joined Pet Simulator 99 (place 8737899170, which
  teleports to its main world 140403681187145). Either aim `OMNI_LATE_TAP`s at Home's tiles and
  the game page's Play button **from captures of the same configuration** (VERIFICATION 19 and 21:
  capture the composed screen with the window in the foreground; the previous session's
  `shoot_both.ps1` does it), or decode how a device launches a place from a link
  (`roblox://placeId=...` / `robloxmobile://`, which is how a browser's Play button reaches the app
  -- the Lua app registers link patterns at startup, see `roblox.*://navigation/share_links.*` in
  the logs) and drive that path the way the Java side does. The link path is worth it if it can be
  decoded within reasonable time: it makes every in-world run repeatable. Your call; say which.
* The network: the owner's ProtonVPN must be up (`Resolve-DnsName clientsettingscdn.roblox.com`
  answers Akamai/CloudFront, not `195.175.254.2`). Without it nothing reaches Roblox; stop and say so.

## The job

1. **Build the instruments the world needs, then measure before changing anything.**
   * Interval reporting (every 5-10 s, beside the FRAMES line), not end-of-session totals, so the
     load and the world are separable.
   * Where a frame's time goes, on the **render thread** and on the threads it waits for:
     - **translation** (time inside dynarmic's compile, per host thread; instructions translated;
       cache evacuations) versus **JIT execution** (guest code);
     - **handler time by symbol**, especially the **Vulkan forwarding** (`vk*` through
       `omni-android/src/vulkan` into `omni-gfx`): per-call cost, and time blocked in
       `vkWaitForFences`, `vkQueueSubmit`, `vkAcquireNextImageKHR`, `vkQueuePresentKHR`;
     - **waits**: which condition variables and futexes the render thread and the job threads
       block on, who wakes them, and for how long;
     - the game loop's spin (guest thread 5 on the menus) and whether it contends anything the
       render path needs.
   * The **~60 s of zero frames while a world loads**: what the render thread is doing (waiting on
     what?), what the busy threads are doing (translating? executing Luau? in handlers?), and
     whether one thread's work serializes everyone else.
   * A repeatable baseline in the world: presents per 5 s and CPU (per-thread
     `TotalProcessorTime` deltas), with the camera idle and with a synthetic drag, n >= 2 runs.
   * Temporary instrumentation must be env-gated, cheap when off, and tidied or removed
     afterwards; keep what is worth keeping as a documented switch.
2. **Fix only what the evidence shows, faithfully.**
   * No faked frame pacing, no skipped guest work, no plausible stubs, no dropped frames passed
     off as presented ones.
   * Candidate levers **only if measured to matter**: sharing translated code between guest
     threads (or otherwise not translating the same code once per thread); dynarmic options
     (`omni-cpu`), the per-slice callback invariant, run-window step budgets; the cost of a
     boundary crossing; lock or wake hand-offs in `omni-bionic`/`bionic`; the Vulkan forwarding's
     own overhead (locks, table lookups, allocations per call). D4 (identity mapping) and D5 (the
     pinned, unmodified dynarmic tree) stand; a change to the vendored tree is a decision to record
     in `docs/DECISIONS.md`, not a side effect.
   * **Before/after numbers from the same scenario**, in the world, with n stated. Add a detector
     (test or mutation row) wherever one can be written.
3. If the engine itself chooses a frame rate (render-on-demand, a throttle), prove it from the
   binary or its logs before spending time on it.

## Constraints

* **Work in the worktree and target directories named in the launch message**, never in the main
  checkout (VERIFICATION 18). Set `OMNIDROID_DYNARMIC_BUILD_DIR` to the dynarmic build dir you are
  given (MSVC fails on long paths) and `CARGO_TARGET_DIR` to your worktree's own `target`.
* **Files you own** are listed in the launch message. Do not edit anything else. In particular
  **`crates/omni-android/tests/gameactivity.rs` belongs to the other agent (input)**: put your
  instruments in library code, enabled by an environment variable and reporting on their own
  (for example a reporter thread started the first time the switch is read). If you truly need a
  line in the gate, write it as a patch in your report and the controller applies it.
* `tools/mutate.py` is shared: add rows only by inserting before the list terminator (never
  slicing), with your own prefix, and run only `--only <your-prefix>`, never while editing, and
  never while a gate run of yours is going. The controller merges the file.
* **Memory is the scarce resource on this host.** 31.8 GB RAM and a ~48.6 GB commit limit, most of
  it held by the owner's apps; a build during a session once exhausted it (error 1455) and killed
  guest threads. One gate run at a time; build with `CARGO_BUILD_JOBS=6`; before a gate run check
  free commit (`(Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory`) and wait if it is under
  ~6 GB. On a 1455 or os error 112 (disk), stop and report.
* **If the controller tells you the owner is in a session, stop building and running until told
  otherwise.**
* No commits by you. Keep the tree compiling at all times.
* Windows x86-64 only. `cfg(target_os)` only inside `omni-platform`. No Vulkan validation layers.
* Scratch tools from the previous session, at
  `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\a4b3c2d9-3e3f-42b2-a30d-3fdb78dfe571\scratchpad`:
  `a64.py <hex> <count>` (capstone disassembly at link addresses), `blcallers.py`, `anyref.py`,
  `addrref.py`, `dynsym.py`, `gotslot.py`, `dexdis.py`/`dexgrep.py`/`dexclass.py` (dex), and
  `perf/` (the menu-era runs and `summarize.py`). Its `play/play17.log` is the in-world run above.

## Report

The baseline (in the world, with n), what a frame's time is spent on (with evidence), each change
with before/after numbers from the same scenario, files changed, tests run (verbatim, whole
affected suites, not filtered), mutation rows and their results, and what remains. MEASURED versus
inferred, precisely.
