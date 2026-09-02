# Handoff: find out what actually limits Roblox in-world

Paste the block at the bottom into a fresh Claude Code session started in
`C:\Users\berat\Desktop\Omni Apps`.

**Supersedes the previous version of this file.** That one asked for patch
0009 to be built. It was built; it disproved its own premise. See
`MODES.md` → "The 60 fps ceiling" for the corrected story.

## Where things stand

| | |
|---|---|
| engine | `omnidroid`, branch `perf/native-speed-pass`, HEAD `aeb71c9` |
| app | `omni-executor`, branch `feature/2captcha-account-creator`, HEAD `eb373a3` |
| deployed | app-win **1.0.37** — do not touch without asking |
| built, inert | `C:\qemu-omni-refresh` (`+omni-refresh`). Not wired in, not deployed. |

### Settled — do not re-measure

* **ARM translation is 1.14x integer / 0.98x SIMD / 1.54x indirect call.**
  `tools/bench/mkbench.py`.
* **Composition is fixed.** The immersive-mode toast was forcing client
  composition on every frame; removing it took clientCompositionFrames 100%
  → 0% and missedFrames 67% → 0.5%. Shipped in 1.0.37.
* **The window's MONITOR sets the guest's present rate.** 92.3/92.5 fps on
  the 144 Hz panel vs 62.4/62.6 on the 60 Hz Parsec Virtual Display at
  x = −1920. GDK reports both correctly; patch 0009's detection is inert.
* **Guest present rate ≠ what you see.** 60.0 host draws/s were logged while
  the guest presented 86 fps. GDK3's frame clock caps the *visible* rate at
  60. Raising the guest rate buys latency, not visible frames.

### THE OPEN QUESTION — this is the whole job

In-world PS99 has been measured twice and does not agree with itself:

```
2026-09-01   47-53 fps   guest 135-185% of 800%
2026-09-02   22-25 fps   guest ~300%, present p50 42-45 ms   (one clean run)
```

Both are **below every display cap**, so the 60 Hz ceiling is not what limits
gameplay. Either the client is producing ~25 and something is badly wrong, or
~50 and the second run was unrepresentative.

**Find out which, and find out what the guest is spending 300% of a core on.**
That answer decides everything downstream; nothing else is worth doing first.

If it is genuinely CPU-bound in-guest, note that this **partly rehabilitates
the translator theory I dismissed**: I ruled it out because compute is
1.0–1.5x *and* the guest was not CPU-saturated. If it is now saturated on
three threads, the 1.54x indirect-call tax lands on exactly the branchy C++ a
game engine is made of. Get per-thread attribution before believing it.

## Method — get this wrong and every number is fiction

* **Use a SPARE account.** The previous session signed in `HezMi_ImYu` and
  kicked the user's own live game, then `stop` powered down their instance —
  both resolve the same runtime entry. Ask which account is free.
* **Move `%LOCALAPPDATA%\OmniExec\autoexec\zaphub.lua` aside, put it back
  after.** It draws a full-screen GUI and pins any reading to a flat ~60 fps
  at ~6% guest CPU regardless of the stack underneath.
* **Keep the QEMU window on the 144 Hz panel** (primary, at 0,0). The Parsec
  display at x = −1920 costs a third of the frame rate.
* **Screenshot before and after every sample** and confirm the 3D world is on
  screen. A loader, a join splash, a 277 disconnect and a sign-in screen all
  produce plausible numbers. `tools/bench/sf-timestats.sh` does this for you.
* **Two samples minimum**, and run the control twice before believing any
  improvement — two "wins" in this project (`blob=true`, process priority)
  evaporated under a repeated control.
* Per-thread attribution: sample `/proc/<pid>/task/*/stat` deltas over a
  window, not `top`, whose %CPU column reads 0 on this base.

## Then, depending on what it shows

* **CPU-bound in-guest** → per-thread attribution first. If Roblox's own
  threads dominate, evaluate a newer translator (Berberis / newer
  ndk_translation is in the android-36 emulator image already on this disk) —
  but read `MODES.md` "The translator is not the wall" first for what that
  costs and why it was rejected before.
* **Not CPU-bound** → find the wait. `--latency` timestamps on the game layer.
* **Only then** consider a GDK frame-clock bypass, and only if in-world ever
  exceeds 60. It is worthless while the game makes 25.

## Optional, small, safe

Make the engine prefer the **highest-refresh monitor** when placing the gaming
window instead of trusting remembered geometry. Worth 60 → 92 guest fps and it
is our code (`place_window` / `hostwin.py`). Ask before shipping it.

## Rules

Do not deploy, publish, or push an app version. Read `omnidroid/MODES.md`
first. Say plainly when a number is one sample.

---

## PASTE THIS

> Read `omnidroid/HANDOFF-FPS.md` first — it is the full brief and it
> supersedes what you may infer from git log.
>
> Short version: OmniDroid's gaming mode has an unresolved in-world frame
> rate. PS99 measured 47–53 fps one day and 22–25 fps the next, at ~300% of
> the guest's 800% CPU. Both are below every display cap, so the 60 Hz
> ceiling is NOT what limits gameplay and chasing it is premature.
>
> Your job: get a trustworthy in-world number and find out what the guest is
> spending that CPU on — per-thread attribution out of `/proc/<pid>/task/*/stat`
> deltas, not `top`. Then tell me what the binding constraint actually is.
>
> The measurement protocol in the handoff is not optional: use a spare account
> (ask me which), move `zaphub.lua` out of the autoexec folder first, keep the
> QEMU window on the 144 Hz primary and not the Parsec display at x=−1920,
> screenshot before and after every sample, and take at least two. Several
> confident numbers in this project have turned out to be a full-screen GUI or
> a loading screen.
>
> Do not deploy, publish, or push an app version. Currently deployed is
> app-win 1.0.37.
