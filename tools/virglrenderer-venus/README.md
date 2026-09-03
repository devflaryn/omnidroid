# virglrenderer + venus, mingw64 build

Rebuilds `libvirglrenderer-1.dll` (virglrenderer 1.3.0) with venus/Vulkan
enabled, for a Windows/msys2 host. Preserved so it can be redone in minutes
if upstream changes — it does not need to be re-run to reproduce today's
result.

**Verdict: NEGATIVE.** The DLL builds (exports byte-identical to stock,
+1.1 MB of venus code) but cannot serve `venus=on` on this host, for two
independent reasons: virglrenderer 1.3.0's venus path is reachable only
through a render server that is fork()/socketpair()/epoll-based (POSIX-only,
no Windows port attempted here), and even if that were solved, venus's host
memory export is fd/dma_buf-only while this GPU's Windows driver only
exposes `VK_KHR_external_memory_win32`. See `docs/bench-2026-09-03.md`,
"Third pass 2026-09-03", for the full evidence (file:line citations, the
measured launch, the probe output).

## Files
- `venus-mingw-1.3.0.patch` — every source change on a clean 1.3.0 checkout
  (the two msys2 patches below, plus render-server-off-on-Windows and the
  getpagesize/mman/dlfcn/setpriority/thrd_current ports).
- `001-void-param.patch`, `002-no-ioccom.patch` — from
  https://github.com/msys2/MINGW-packages/tree/master/mingw-w64-virglrenderer.
  Already folded into `venus-mingw-1.3.0.patch`; kept standalone for
  reference only — do NOT `git apply` them separately, only apply
  `venus-mingw-1.3.0.patch` (applying 001/002 first double-applies both
  hunks and the folded patch fails).
- `vkprobe.c` — host Vulkan extension probe (`cc vkprobe.c -lvulkan-1 -o
  vkprobe` under MSYS2 MINGW64); header comment carries what it printed here.
- `build.sh` — deps, clone @ 1.3.0, apply `venus-mingw-1.3.0.patch`,
  meson/ninja build, install, discriminating checks — one script.

## Traps
- `strings <dll> | grep -ic venus` is **degenerate**: it returns 1 for the
  stock, venus-less DLL too (one string compiled in regardless of
  `ENABLE_VENUS`). Use `objdump -p <dll> | grep -c vulkan-1.dll` or the
  export list instead.
- `-Dplatforms=` (even empty) silently drops 18 `virgl_egl_*` exports the
  existing GLES/D3D11 path depends on. Do not pass it — let `platforms=auto`
  stand, as the msys2 PKGBUILD does.
- Do not ship mingw's `/mingw64/bin/vulkan-1.dll` next to the built DLL. It
  only imports `vkGetInstanceProcAddr`; the loader search order must resolve
  that to `C:\Windows\System32\vulkan-1.dll` (the real ICD loader).
