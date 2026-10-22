# Morning report -- 2026-10-22, branch `perf/endless-1022` (pushed, not merged)

The ask: keep making omnidroid boot faster and use less RAM, CPU and disk, until stopped. Work is on
`perf/endless-1022` (worktree `C:\oe`); `main` is untouched and this checkout is back on it.
Every number below is from an interleaved A/B on this PC; the ledger
(`docs/NIGHT-2026-10-22-endless.md`) has each run.

## Take first: two fixes

1. **`302e04b5` -- saved devices survive Windows' temp cleanup.** At ~07:00 Windows' empty-folder
   cleanup (C: at ~20 GB free) deleted `/data/apex/sessions`, `/data/tombstones`, ... from **every**
   saved device in `%TEMP%\omni-golden` -- the same as on 2026-10-03. `.omni-init-dirs` still said
   they were made, so apexd aborted at every boot and the warm device never came up. **Your pool's
   saved devices were among them**: I set them aside as `*.gutted-20261022` (nothing deleted of
   yours), so your next pool boot makes a fresh one (slower once). The fix re-makes any of init's
   directories that are gone at every boot.
2. **`63029a0f` -- the system host stops growing with every app launch.** The binder broker never
   counted a live process's handle references, so system_server never let go of anything -- every
   SurfaceFlinger layer of every app ever launched stayed, with its last buffers and their GPU
   mirrors: **~17 MiB of the system host per launch, forever** (an agent testing 100 APKs: ~1.7 GB).
   Now counted as the kernel counts them; after 8 launches 812-817 -> 748-754 MiB, after 25 flat at
   743 MiB. `OMNI_BINDER_REFCOUNT=0` is the old way.

## Where things stand (saved warm device, Windows i7)

| | main (start of the night) | branch now |
|---|---|---|
| warm ready (boot from saved device) | 15.3 / 17.2 / 16.7 s | **~11.3-11.9 s** |
| idle warm Android, private bytes, all hosts | ~1,470-1,520 MB | **~1,136 MB** (4 min after ready) |
| disk per saved / running device | 248 MB | **~10 MB** (APEXes linked) |
| system host after 8 app launches | +~110 MB and climbing | flat |

## What changed (each its own commit)

- s1 decompressed APEXes hard-linked, not copied: -237 MB disk a device.
- s2 the kiosk device's own home (`com.omnidroid.home`, software-drawn) instead of Settings'
  FallbackHome: -170..-220 MB while the home is in front (idle warm devices).
- s4 inert overlays (emulator skins, cutout emulation, telephony's) left out: warm ready -1.3 s.
- s6 the boot script waits on the host's property service, not `sleep 1` polling: -0.3 s.
- s7 20 boot receivers a kiosk has no use for disabled (init's `omni_quiet`, as root; the pool per
  user): 4 fewer app processes after each boot.
- s8/s11 the home starts in a spare app process started with system_server: -1.0 s (and -0.2).
- s9 the spare polls every 50 ms, not 10: idle CPU -2 points of a core.
- s12 init skips `apexd --bootstrap` (fails here) and unstaged root scripts: init -140 ms.
- s13 `/linkerconfig` made once per image: init -410 ms.
- s16 binder references counted (above).
- s17 no notification assistant: one app process (~80 MB) less a warm Android and a pool user.

Not kept (measured, reverted): system_server without class preload (s5), idle vsync throttling
(s10: vsync callbacks are cheap), the BPF loader beside the boot (s14: init shorter, boot not),
SurfaceFlinger blur off, fat LTO (-1.6% size).

## For you to decide

- **Translation snapshots** (`OMNI_JIT_SNAPSHOT=1`): now worth -0.5..-0.8 s of boot once filled
  (two runs), and **idle CPU ~-2.5 points of a core**, for +30..+130 MB. Still your call.
- **Main**: merge the two fixes first; the rest is one branch, each commit separable.
- Not checked: a live pool with users (your pool held the machine's one pool all night), the Mac
  and Linux hosts, and an in-game Roblox run -- both Delta APKs on the Desktop stop at Android's
  "All files access" page (they ask for MANAGE_EXTERNAL_STORAGE without declaring it; the same on
  `main`), so no game could be joined. The probe app and Roblox up to that page behave as on `main`.
