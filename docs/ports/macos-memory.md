# macOS: memory at the landing screen

What `tools/footprint_mac.py` measured (1 s samples of `phys_footprint` and the kernel's lifetime
peak, from outside the process; `vmmap` at +60 s) on the M1, running the gate to the engine's
landing screen, before any world. In-world figures (2.7-2.9 GiB) are in `macos.md`.

## Result of patches 0010-0013 and the released load buffer

| | before (`ef65a2d`, n = 3) | after (n = 4) |
|---|---|---|
| footprint at +60 s | 2,420-2,698 MiB, still climbing | **811-841 MiB**, flat |
| boot peak (`ri_lifetime_max_phys_footprint`) | 2,492-2,878 MiB | **1,045-1,122 MiB** |
| dynarmic bookkeeping per translated block | 2,099 B | 322 B (by capacity) |
| block records the jits hold at +60 s | 593,699, 54% invalidated | 331,691, 4% invalidated |
| whole-`libroblox.so` buffer held for the session | 104 MiB | 0 |

Root causes, one patch each: block records kept whole inside map buckets (0010: flat 24-byte
records); an interval map per jit never cleared (0011); invalidated blocks kept until the cache
filled (0012: an invalidation that leaves nothing is a clear); freed large arrays kept by the host
allocator (0013: page-backed arrays). The gate reads `libroblox.so` into a `LoadBytes` released
after the load. Most invalidations came from one source: `omni-android` sent every guest
`munmap`/`mprotect` to every thread, and a thread whose queue overflowed invalidated its whole
space.

After, by `vmmap` category (n = 3): guest space 324-326 MiB (the engine's own; the floor),
`MAP_JIT` code caches 169-184 MiB (one translation per guest thread), host heap ~135 MiB
(unattributed), graphics ~85 MiB, dynarmic bookkeeping ~100 MiB.

## Several instances

`footprint_mac.py --launch 4 --stagger 90 --session 330` (each its own `OMNI_DATA_DIR`, 16 GB Mac
with other apps open): 4 of 4 reached the landing screen and exited 0, 750-860 MiB each, 3,171 MiB
together; free memory 62% -> 35%, the compressor grew ~4 GB, swap never moved. Ten instances on
8 GB is not shown. In two earlier runs an instance failed on the network (`SslConnectFail`, a
refused `getaddrinfo`) within a second of another starting; not investigated.

## Open

* The code caches' touched pages stay dirty after a clear; a shared cache per process (D38) is
  x64-only, and the arm64 design would need per-thread values (`TPIDR` boxes, monitor slots,
  callbacks) loaded from `JitState` instead of embedded, a locked lookup, and quiescence before
  reuse.
* `MALLOC_LARGE (empty)` 57-61 MiB, kept by the allocator after someone freed it; not attributed.
