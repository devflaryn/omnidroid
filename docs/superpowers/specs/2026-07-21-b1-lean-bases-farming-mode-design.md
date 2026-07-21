# Omnidroid B1: Lean Bases + Farming Mode — Design

**Date:** 2026-07-21
**Status:** Approved design, not yet implemented
**Repo:** omnidroid (engine). Spec B of the omni-apps brainstorm; B1 of two (B2 =
playable GPU-rendering mode, a separate later spec).

## Problem

Omnidroid instances are slow to boot and heavy. The user wants two modes:
- **playable** — the good-looking, smooth, higher-RAM experience (GPU rendering
  is B2, out of scope here);
- **farming** — headless, squeezed as small as possible so many instances run
  per host. Target: a **joined-but-idle** instance under **~400MB** guest RAM.

Three original issues collapse into one effort: farming footprint (#1), boot
time (#2), and trimming unused OS features (#3) are all served by making the
base image lean. The current leanest mode (`brutal`) is 2GB — 5× over target —
so <400MB is fundamentally an image-slimming + runtime-squeeze problem, not a
QEMU-flag tweak.

## Scope & decisions (locked in brainstorming)

- **B1 only** — lean/fast/farming. Playable-GPU (B2) is a separate spec; the
  `playable` mode is left as-is here (it's where B2 lands later).
- **One trimmed base per arch**, `farming`/`playable` are RUNTIME modes on top.
  The trim helps boot time, storage, AND baseline RAM at once. Split to a
  dedicated farming image ONLY if measurement proves the unified approach can't
  reach target (the Phase-3 decision gate). YAGNI until then.
- **Both arches in scope: arm (LineageOS) + x86 (Bliss).** x86 image work is
  built FROM the ARM Mac accepting slow TCG iteration — a one-time build cost,
  not a runtime cost (the flattened x86 image runs normally on x86 hosts). Not
  skipped.
- **Dev base is ALSO trimmed** (arm-only: `base_arm_devsystem` + devkit), lower
  priority than prod, done last, with a stricter floor (must preserve the
  devkit).
- **Farming state = joined but idle/minimal** — logged in and in a place, but
  the game is backgrounded/low-activity (no active rendering, minimal
  scripting). <400MB is only plausible in this state; an actively-rendering
  Roblox exceeds 400MB by itself.
- **Measurement APK + place (2026-07-21 update):** the base's currently
  pre-installed Roblox is FLAGGED and boots to a black screen — useless for RAM
  measurement. All B1 measurements MUST instead install and measure the
  bootstrap APK at `~/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk`,
  and must be taken while **joined to place id `8737899170`** (in-place,
  joined-idle) — NOT on the home screen. A home-screen or black-screen RSS is
  not a valid B1 measurement.
- **Mixed production hosts** — x86 Windows/Linux and ARM Macs. B1 stays
  host-agnostic; the <400MB *guarantee* is tightest on Linux/KVM and
  best-effort on macOS/HVF. The design reports the real measured number
  per-host rather than promising one figure everywhere.
- **Surgical trim, not rebuild-from-source.** Remove unused apps/services from
  the known-good bases and tune memory; keep the verified Roblox/kiosk/root
  setup intact.
- **~400MB is the target to push toward**, phrased as "reach <400MB or
  as-low-as-stable" — the success metric is many more instances per host than
  today, proven by measurement.

## Architecture: a measure-first spine

Every change is bracketed by measurement, because <400MB is a claim to be
proven per-host, not assumed.

```
Phase 0  BASELINE MEASURE   boot each base, join Roblox, idle -> record RSS +
                            boot time (arm on HVF; x86 where it runs). The
                            number every later phase is compared against.
   |
Phase 1  SURGICAL TRIM      per base (prod arm, prod x86, then dev arm):
                            boot -> adb remount rw -> remove unused apps +
                            disable unneeded services -> flatten to a NEW base
                            version (old retained; bases are immutable once an
                            account references them). Re-measure boot + baseline
                            RSS. Each removal batch gated by the base's floor
                            smoke-test.
   |
Phase 2  FARMING MODE       new MODES entry (low mem cap + fewer vCPUs) + a
                            post-boot runtime squeeze (stop residual services,
                            throttle the backgrounded game, zram + lmkd tune).
                            Re-measure joined-idle RSS.
   |
Phase 3  DECISION GATE      farming reached <400MB (or as-low-as-stable)?
                            YES -> done, one base per arch.
                            NO  -> cut a dedicated stripped farming image, now
                                   with data justifying it.
```

## Component A — Surgical trim (Phase 1)

**Mechanism (reuses the existing base-rebuild path, per-arch aware):** the
engine already boots a base, `adb shell mount -o remount,rw /`, modifies
`/system`, and flattens the overlay to a new self-contained image
(engine.py:2522-2558; the base-rebuild flow). The trim is that same flow with
removals + service-disables, in **incremental batches**, each gated by a floor
smoke-test, flattened only when green, reverted if not. Prior base version
always retained. (Note: `cmd_update_base` at engine.py:2443 is a DIFFERENT
thing — account overlay migration, which refuses on arm — do not confuse it
with the base-modify path.)

**Per-arch asymmetry (a real constraint, not a detail):**
- **x86 (Bliss)** carries the kiosk and system apps in `/system`; it supports the
  `adb root` + remount-rw + `/system` flatten flow directly. Trimming x86
  `/system` is straightforward with that path.
- **arm (LineageOS)** is a `user` build with **no `adb root`**, and its
  kiosk/apps live in the **`/data` template** (`base_arm_data.qcow2` /
  `base_arm_devdata.qcow2`), not `/system` — refreshed via the
  `update_kiosk_arm` pattern (boot a throwaway account, change over adb, copy
  `/data` back over the template as a matched FBE pair). So arm trimming splits
  by target: **`/data`-template contents** (removable via the throwaway-account
  + copy-back pattern) vs **`/system`/`/product` contents** (the pre-installed
  Roblox lives in `/product/app/Roblox`; removing *other* `/system`/`/product`
  bloat needs the rebuild flow's rw path, constrained by no-`adb-root` and the
  matched-pair FBE requirement — verify the FBE pair still decrypts after each
  arm flatten). The trim plan must treat arm `/data` and arm `/system` as two
  distinct, separately-gated batches.

**Per-base requirement FLOORS** (the trim may remove anything NOT needed by the
floor):

- **PROD floor** (`base_arm`, `base_x86`): boots to the kiosk -> launches the
  single baked/installed Roblox APK -> adb reachable on a FRESH boot
  (`androidboot.insecure_adb=1`) -> cookie login works -> VNC view works ->
  device-owner / Lock-Task kiosk lockdown intact.
- **DEV floor** (`base_arm_devsystem`): everything in PROD, PLUS frida-server
  reachable on the hidden port (27142), Magisk `su` works, the devkit vdc disk
  mounts + activates (`_devkit_activate`), the dev-UI/kiosk launcher toggle
  works, always-on screenshots work. A trim that boots but breaks `su` or frida
  is a FAILED dev trim.

**Candidate removals** (validated one batch at a time against the floor): stock
apps the kiosk replaced (browser, email, gallery, stock launcher, setup
wizard); telephony/dialer/contacts/SMS if unused; printing; camera/AR services;
NFC; unused input methods; extra locales; unused HALs + their daemons;
sample/demo content. The list is a starting hypothesis — each batch is proven
against the floor, not assumed safe.

**Anti-brick guarantees:** incremental batches; floor smoke-test gate per batch;
flatten to a NEW versioned image (`base_arm_v3`, `base_x86_v6`,
`base_arm_devsystem_v3`); prior versions retained (existing account overlays are
COW-backed by them — never delete a referenced base); the measure-first spine
catches an RSS/boot regression immediately.

**x86-from-ARM:** the modify/flatten path is adb-driven and host-agnostic;
booting Bliss x86 on an ARM Mac runs under slow TCG (no HVF for x86 on ARM), so
iteration is slow but feasible. The tooling stays host-agnostic so the identical
trim can run on an x86 host if desired. Accepted one-time cost.

## Component B — Farming runtime mode (Phase 2)

**Static — a `MODES` entry** (engine.py:1155; `resolve_mode` at 1163 already
plumbs mem/smp into the QEMU command):
- `farming`: low memory cap (start aggressive, e.g. `mem: 512`, measurement sets
  the real floor) + fewer vCPUs (e.g. `smp: 2`).
- `playable` unchanged (B2's GPU work lands there later). `DEFAULT_MODE`
  unchanged.

**Runtime squeeze — after boot_completed, over adb, each step measured:**
- Stop residual services the trim left running but farming doesn't need.
- Throttle the backgrounded Roblox process: CPU cgroup cap + Android
  background/idle policy, so a joined-idle instance does minimal work. Safe
  because instances are headless (`-display none` always) — no active render
  surface to fight.
- Memory pressure: `zram` swap + `lowmemorykiller`/`lmkd` thresholds tuned so
  the guest reclaims hard under the low cap WITHOUT OOM-killing Roblox itself.

The squeeze is a well-formed adb command sequence (host-side testable for
shape) applied at/after boot; it does not require a separate image.

## Component C — Measurement harness (the spine itself)

A small, reusable measurement path (may extend the existing `cmd_bench_ksm` /
`cmd_ksm` tooling at engine.py:4263/4324) that, for a given base+mode:
1. boots an instance, drives it to joined-idle Roblox,
2. records **boot time** (kernel start -> boot_completed -> game process up) and
   **guest RSS** (and host-side process RSS),
3. emits a machine-readable JSON row so Phase 0/1/2 deltas are comparable.

**Stale-QEMU guard (mandatory):** before trusting any measurement, the harness
`ps`-checks for stray `qemu-system-*` the engine may have lost track of (the
known open bug: a live qemu that `list` reports as `stopped`; tell = "boot
completed after 0.0 min" vs a real ~0.4-0.6 min arm boot). A measurement taken
against a mis-attached instance is discarded, not recorded — otherwise a "great"
farming number could be a different instance entirely.

## Verification

**Host-side unit tests (offline):**
- `resolve_mode("farming")` returns the intended mem/smp; `MODES` shape intact;
  `playable`/`DEFAULT_MODE` unchanged.
- The runtime-squeeze command sequence is well-formed (correct adb invocations,
  no missing steps).
- Base-version registration bumps correctly on a trim flatten (new version
  recorded, prior retained, account COW backing not broken).
- Measurement-harness JSON row shape.

**Live, on-device (the real proof, per arch):**
- Phase 0 baseline vs post-trim vs farming RSS + boot time, joined-idle — every
  claim a measured delta.
- PROD floor smoke-test green on trimmed `base_arm` and `base_x86`.
- DEV floor smoke-test green on trimmed `base_arm_devsystem` (incl.
  frida/Magisk/vdc/toggle/screenshots).
- The Phase-3 decision gate, decided on the measured joined-idle farming RSS.

## Baseline facts (measured 2026-07-21, for reference)

External images in `~/OmniImages` (never committed; `configs/paths.json` is the
registry): `base_arm.qcow2` ~2.3G + `base_arm_data` ~1.0G (prod arm, LineageOS
23.2); `base_x86.qcow2` ~2.7G (prod x86, Bliss 16.9.7);
`base_arm_devsystem.qcow2` ~1.1-2.1G + `base_arm_devkit.qcow2` ~37M (dev arm,
frida 17.15.4 + Magisk v30.7). Clear disk-slimming headroom on all three.

## Risks

- **<400MB may be infeasible even joined-idle** on some hosts (esp.
  macOS/HVF, coarser memory control than Linux/KVM cgroups+zram). Mitigated by
  the "as-low-as-stable" framing and the Phase-3 gate — the design surfaces the
  real number and, if needed, falls back to a dedicated farming image.
- **A trim batch bricks or degrades a base.** Mitigated by incremental
  floor-gated batches + retained prior versions.
- **arm trim breaks the matched-pair FBE decrypt.** arm system+data are an FBE
  matched pair; a `/system` change that desyncs the pair makes `/data` fail to
  decrypt (the base won't boot to a usable state). Mitigated by re-running the
  floor smoke-test (which includes a clean boot to kiosk) after every arm
  flatten, and by the no-`adb-root` constraint limiting how deep arm `/system`
  edits can go — arm bloat that lives in `/data` is trimmed via the safer
  throwaway-account + copy-back template path instead.
- **Throttling the game too hard breaks "joined"** (disconnect/AFK-kick).
  Farming throttle must keep the session alive; the joined-idle smoke-test
  checks the session is still connected after the squeeze.
- **Measurement corrupted by the stale-QEMU bug** — mitigated by the harness
  `ps` guard above.
- **x86-on-ARM iteration is slow.** Accepted; one-time build cost.

## Out of scope

- **B2 — playable GPU-rendering mode** (reviving a GL/virgl path; depends on
  host GPU). Separate spec. `playable` mode is untouched here beyond coexisting.
- **The stale-QEMU detection bug fix itself** — B1 only GUARDS its measurements
  against it; the root fix is separate omnidroid work.
- **Non-Roblox workloads.** Farming is tuned for the joined-idle Roblox case.

## Sequencing

1. Component C (measurement harness + stale-QEMU guard) — needed to prove
   everything else; Phase 0 baseline uses it.
2. Component A trim — prod arm, then prod x86, then dev arm (each floor-gated).
3. Component B farming mode — MODES entry + runtime squeeze.
4. Phase-3 decision gate on the measured farming RSS.
