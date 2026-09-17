# Omnidroid documentation

## Design
- `ARCHITECTURE.md` — runtime core, APK/ELF loading, Android compatibility layer, ARM64
  execution, memory model, isolation, graphics, platform abstraction, testing strategy.
- `STATUS.md` — what is verified working vs. planned vs. untested. Kept honest.

## Research
Findings gathered before designing, so the design rests on measurements rather than assumptions.

- `research/host-environment.md` — the development machine, measured (CPU features, toolchain,
  Vulkan). **Verified.**
- `research/prior-art.md` — Sober and other reimplementation projects, their real architectures
  and licenses, and what can legitimately be reused.
- `research/apk-analysis.md` — forensic inspection of `Roblox-2.738.1397.apk`.
- `research/windows-memory-model.md` — measured behaviour of the Windows virtual memory APIs
  that the demand-driven memory model depends on.
- `research/graphics-spike.md` — measured host Vulkan and windowing capabilities.

## Conventions
Every document states how each claim was established. Claims that were not verified by running
something are marked as inferences or as unverified.
