# B2 Spike — does the guest render with a real GPU? (run on the ARM Mac)

**Goal:** find out, with ONE experiment, whether Roblox renders with real
graphics acceleration in a native window. This single result decides the rest
of B2 — whether it's a small job (build the playable mode) or a bigger one
(B3: add graphics drivers to the image).

You do NOT need to understand the internals. Just follow the steps and write
down what you see.

---

## What you'll do (overview)
Start one instance with the experimental accelerated window turned on, get
Roblox running and joined to a place, and look at how it renders.

---

## Steps (copy/paste each command)

**1. Pick a logged-in account name** you already have (from `omni login`).
Call it `ACCT` in the commands below. If you have none yet, run `omni login`
first and use that username.

**2. Start it WITH the accelerated window.** The only difference from a normal
start is the `OMNI_GL_WINDOW=1` prefix:

```sh
OMNI_GL_WINDOW=1 python3 -m omnidroid start ACCT --mode playable
```

A QEMU window should open on your Mac's screen.
- If **no window opens**, or QEMU prints an error mentioning `cocoa`, `gl`, or
  `virtio-gpu-gl` → that itself is a finding. Copy the error into the RESULT
  section below and skip to "Recording the result".

**3. Install the bootstrap APK** (NOT the flagged pre-installed Roblox — that
one black-screens). Open a second terminal and run:

```sh
adb -s 127.0.0.1:16001 install -r "$HOME/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk"
```

- `16001` is the default adb port of the first instance. If you started a second
  instance it's `16002`, and so on — `omni list` shows each instance's ports.

**4. Get into the game.** In the window, let Roblox open and log in (the
bootstrap cookie logs you in automatically), then **join place id
`8737899170`**. It should deep-link into that place; if it lands on the home
screen instead, open that place from there.

**5. LOOK at the game in the window** and pick the verdict:

| What you see | Verdict |
|---|---|
| Renders smoothly, looks like real 3D | **ACCELERATED** ✅ |
| Renders but choppy/laggy, slideshow-like | **SOFTWARE** ❌ |
| Black screen, game never draws | **BLACK** ❌ |

---

## Recording the result

Fill this in, then commit this file (`git add` + `git commit`):

- **Window opened?** (yes/no + any QEMU error):
- **Roblox installed + logged in + joined `8737899170`?** (yes/no):
- **Render verdict** (ACCELERATED / SOFTWARE / BLACK):
- **Notes** (how the framerate felt, anything odd, any anti-cheat/kick behavior):

### Partial result (2026-07-21, INCONCLUSIVE — pending re-run)
- The accelerated window opened and the Roblox MENU already felt **noticeably
  smoother** than software rendering — an early positive (ACCELERATED-leaning)
  signal.
- BUT Roblox pushed a NEW version, so the current bootstrap APK hit an update
  error and could not join place 8737899170 yet. Full verdict is DEFERRED until
  omni-agent patches the new Roblox version into a fresh bootstrap APK; then
  re-run steps 2-5 and record the real joined-in-place verdict.
- Treat B2 as "leaning green, unconfirmed" until the joined re-run is done.

---

## What the result means (what happens next)

- **ACCELERATED** → the guest already has what it needs. B2 continues with
  Tasks 3–5 of the plan (build the real, compatibility-safe playable mode).
- **SOFTWARE or BLACK** → the guest is missing a graphics driver. **Stop B2
  here.** The next step becomes Task 3-ALT: write down exactly what happened and
  open a **B3** plan for the bigger job (adding a graphics-driver stack to the
  image). We would design that separately, with this result in hand — not dive
  in blind.

Either way, this experiment is safe and reversible: the `OMNI_GL_WINDOW` switch
does nothing unless you set it, so your normal `omni start` and all farming
instances are completely unaffected.
