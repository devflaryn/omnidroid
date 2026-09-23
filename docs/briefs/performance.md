# Brief: why the Roblox client runs at 1-7 frames per second, and fixing it

Written 2026-09-23 for a subagent; the first attempt was stopped when its session ended, while it
was still reading the docs. Relaunch it with this text (adjust the file-ownership list to whoever
else is editing at the time).

The person using the runtime says it is "unstable and unusable", and the frame rate is the main
complaint. Read `docs/HANDOFF.md` from "# START HERE" and `docs/VERIFICATION.md` first (how
measurements here have gone wrong before; trust runs over summaries).

## What is already measured (2026-09-23)

- **The gate:** `cargo test -p omni-android --release --test gameactivity -- --nocapture
  --test-threads=1 initialize_native_code`, with `OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1
  OMNI_SESSION_SECONDS=<s>`, opens a real window and runs the APK to the Roblox landing screen.
  - Every 5 s it prints `FRAMES: +Ns into the session, P presents (+d in the last 5s)`.
  - `OMNI_PROFILE=1` prints, per guest thread, the share in guest code versus each handler, and the
    crossings per second.
  - `OMNI_LATE_INPUT=40,41,...` makes a synthetic 240 px drag at each second listed.
  - Other switches are at the top of `tests/gameactivity.rs`.
- **Frame rates:**
  - The settled landing draws ~1 present/s: 57 a minute, never 0 in a 5 s window.
  - Loading bursts reach 108-188 per 5 s (~20-37 fps).
  - With a drag every second: 10-38 per 5 s (2-7 fps).
  - The person sees 1-2 fps.
- **CPU is not saturated:**
  - The process uses 2.3 cores idle and 2.4 while dragging (per-thread `TotalProcessorTime` deltas
    over 10 s via PowerShell).
  - One host thread runs at ~97%; the others at ≤46%.
- **The 97% thread is guest thread 5, the engine's GameActivity game loop.**
  - DECODED at link `0x2bcd648`:
    `flags = (this+8 && this+9) ? this+10 : 0; r = ALooper_pollOnce(flags ? 0 : -1, ...);`
    `if r >= 0 handle the event; else if flags DoFrame(this+0x40)`.
  - With the app visible it polls with timeout 0 and spins. `DoFrame` (`0x2bd1cf0`) is a
    mutex-guarded state machine that does nothing in the steady state (only states 3, 5 and 9 act).
  - Profile: 2.5-3.6 million crossings/s, split ~30% `ALooper_pollOnce`, ~45% mutex lock/unlock and
    ~25% guest code. It does not draw.
- **The other threads mostly wait:**
  - Guest thread 6, the one issuing the Vulkan calls, spends 81% in `pthread_cond_wait`.
  - Workers spend 60-99% in `syscall` (the raw futex).
  - Several threads show ~50% guest code and ~50% `pthread_cond_wait`.
- **Timer resolution:**
  - Windows' default tick is ~15.6 ms, which rounds every short sleep or timed wait up.
  - Commit `1634316` holds `timeBeginPeriod(1)` for the gate session
    (`omni_platform::clock::TimerResolution`).
  - **Its effect on the frame rate is NOT measured.** A/B it by disabling that guard line in
    `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`.

## The job

1. **Measure before changing anything.**
   - Establish a repeatable baseline: idle fps, drag fps and CPU, two runs each.
   - Find **what each frame waits on**:
     - which condition variables and futexes thread 6 and the workers block on, who signals them,
       and how long each hand-off takes;
     - whether timed waits and sleeps dominate (requested versus actual duration);
     - whether the render thread waits on the GPU: fences, `vkQueuePresentKHR`, and
       `vkAcquireNextImageKHR` under the FIFO present mode;
     - whether the game loop's spin contends a lock the render path needs (it takes a mutex a
       million or more times a second);
     - whether the JIT is slow on the hot guest paths: dynarmic options in `omni-cpu`, the
       per-slice callback invariant, run-window step budgets, and the cost of each boundary
       crossing.
   - Temporary instrumentation must be env-gated, and tidied or removed afterwards.
2. **Fix what the evidence points to, faithfully.**
   - No faked frame pacing, no skipped guest work, no plausible stubs.
   - Host-supplied values must be true of this host, decoded from the binary, read from the APK,
     or supplied by the embedding.
   - Report before/after numbers from the same scenario, and add a detector wherever one can be
     written.
3. **If the engine itself chooses 1 fps on an idle UI** (its own render-on-demand), prove that from
   the binary or the logs, then focus on the interactive rate (drags, taps).

## Constraints

- Keep the tree compiling at all times.
- Whoever runs this owns the gate runs for the duration: one run at a time, no other gate runs in
  parallel.
- No commits by a subagent.
- `tools/mutate.py` only with `--only <own-new-prefix>`, and never while editing.
- Disk is tight: no extra target directories or worktrees. On os error 112, stop.
- Windows x86-64 only. `cfg(target_os)` only inside `omni-platform`. No Vulkan validation layers.
- Scratch tools (in the 2026-09-23 session scratchpad, also easy to rewrite):
  - `a64.py <hex> <count>` (capstone disassembly at link addresses)
  - `blcallers.py <hex> [--b]`, `anyref.py`, `addrref.py`, `dynsym.py`, `gotslot.py`
  - `rungate.sh <n> <secs>`

## Report

The baseline, what each frame waits on (with evidence), each change with before/after numbers,
files changed, tests (verbatim), and what remains. MEASURED versus inferred, precisely.
