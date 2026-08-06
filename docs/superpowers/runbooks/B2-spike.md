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

### RESOLVED (2026-08-06) — the spike could not have worked as written

The experiment above never needed the Mac to answer it; the QEMU binary
answers it directly, and the answer is that this host has no OpenGL at all:

```
$ qemu-system-aarch64 -display cocoa,gl=on
qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
$ qemu-system-aarch64 -device help | grep gpu
name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"        # no -gl variant
$ brew info virglrenderer
Error: No available formula with the name "virglrenderer".
```

`virtio-gpu-gl` is not a device model on this build, so
`OMNI_GL_WINDOW=1 omni start` produced a command QEMU exits on rather than a
window. Whatever was seen on 2026-07-21 as "the menu felt smoother", it was
not the GL path — that command could not have started.

**So the verdict is neither ACCELERATED nor SOFTWARE/BLACK: the experiment was
untestable on this host, and the blocker is host-side, not guest-side.**

What replaced it (see `MODES.md` and the 2026-08-06 CHANGELOG entry): the
window path is now capability-detected with three tiers — `gl`, `window`
(native window, software rendering) and `none` — and this host lands on
`window`, which is live and verified. `omni start <acct> --mode gaming` is the
supported command; `OMNI_GL_WINDOW` remains as an alias.

Getting a real answer to the ORIGINAL question now needs, in order:

1. a QEMU built `--enable-opengl --enable-virglrenderer` (source build on
   macOS; virglrenderer has no Homebrew formula), then
2. the guest question, which is the real B3: LineageOS arm64 renders in
   software here, so even a virgl-capable QEMU needs a guest driver stack that
   can drive it.

Step 2 is the larger piece and is unchanged by any of this. The Task-1
apparatus stays as the reproduction switch.

### Partial result (2026-07-21, INCONCLUSIVE — superseded by the above)
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
