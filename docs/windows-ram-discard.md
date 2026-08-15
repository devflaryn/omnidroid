# Giving ballooned pages back on Windows

**Status: not built. The patch below is written and reviewed against QEMU
11.0.50's source; it has NOT been compiled or measured, because this host has
6.3 GiB of free disk and a QEMU build tree needs several times that. Anyone
with a build environment can take it from here — and should read "What it can
and cannot buy" first, because the number it changes is smaller than it
looks.**

## The problem, restated precisely

`omnidroid` inflates a virtio-balloon after boot so a farming instance hands
its spare RAM back. On Linux that works end to end: the guest frees the pages,
QEMU calls `ram_block_discard_range()`, `madvise(MADV_DONTNEED)` drops them,
and the HOST's RSS falls. On Windows the last step does not exist, so the
guest gives the pages up and the host keeps paying for them.

Measured on this host, 2026-08-15, PS99, `--mode farming`:

| | |
|---|---|
| guest MemTotal after the balloon | 830 MB (from `-m 2048`) |
| **host RSS of that QEMU** | **2190 MB** |

The balloon did exactly what it promised inside the guest and bought the host
nothing.

## Where it fails, in QEMU's own source

`system/physmem.c`, `ram_block_discard_range()`. The whole reclaim is behind
`CONFIG_MADVISE`:

```c
    if (rb->page_size == qemu_real_host_page_size()) {
#if defined(CONFIG_MADVISE)
        ret = madvise(host_startaddr, length, MADV_DONTNEED);
#else
        ret = -ENOSYS;
        error_report("%s: MADVISE not available", __func__);
#endif
    }
```

Windows does not define `CONFIG_MADVISE`, so every discard returns `-ENOSYS`.
That is also why free-page-reporting had to be dropped there: it produced
~925 failed discards a minute and 78 KB of `qemu.log`, and reclaimed nothing.

## The patch

Windows has had the exact primitive since 8.1: `DiscardVirtualMemory()`
removes committed private pages from the working set and frees the physical
frames, keeping the reservation and the protection — which is `MADV_DONTNEED`
in all the ways that matter here (next touch reads zeroes, no pagefile write).

```diff
--- a/system/physmem.c
+++ b/system/physmem.c
@@
 #include "qemu/osdep.h"
+#ifdef _WIN32
+#include <memoryapi.h>
+#endif
@@ int ram_block_discard_range(RAMBlock *rb, uint64_t start, size_t length)
     if (rb->page_size == qemu_real_host_page_size()) {
 #if defined(CONFIG_MADVISE)
         ret = madvise(host_startaddr, length, MADV_DONTNEED);
+#elif defined(_WIN32)
+        /* DiscardVirtualMemory is MADV_DONTNEED's equivalent: the pages
+         * leave the working set and the frames are freed, the reservation
+         * and protection stay, and the next access reads zeroes. It needs
+         * page-aligned bounds, which the caller has already enforced. */
+        ret = DiscardVirtualMemory(host_startaddr, length) == ERROR_SUCCESS
+              ? 0 : -EINVAL;
 #else
         ret = -ENOSYS;
```

Two things a builder must check that reading the source cannot answer:

1. **WHPX may pin the range.** Guest RAM is handed to the hypervisor with
   `WHvMapGpaRange`, and if that locks the pages then `DiscardVirtualMemory`
   either fails or is undone by the next EPT fault. This is the single
   question that decides whether the patch works at all, and it can only be
   answered by running it. Measure the QEMU process's RSS before and after a
   balloon inflation, not the guest's `MemFree`.
2. **`MEM_RESET` is not a substitute.** `VirtualAlloc(..., MEM_RESET, ...)`
   is the Windows-2000-era relative and only marks the pages as not worth
   preserving; it does not shrink the working set, which is the number this
   whole exercise is about.

Build notes: MSYS2/mingw-w64, `./configure --target-list=x86_64-softmmu`,
and the result has to be re-cut into the pinned portable build
(`omni-backend/scripts/build-qemu-portable.py`) because the product ships
QEMU 11.0.50 rather than whatever the vendor installer currently offers.

## What it can and cannot buy

It closes the gap between what the guest gives back and what the host gets.
It does not change what the guest needs in the first place, and on the stated
target — 30 instances of PS99 in 32 GB — that second number is the binding
one.

Measured 2026-08-15 on PS99 (place 8737899170), one instance, in-world
loading, `dumpsys meminfo com.roblox.client`:

| | |
|---|---|
| game PSS at the login screen | ~500 MB |
| **game PSS loading the world** | **1018 → 1476 MB, then the client was killed in a 2048 MB guest** |

So one PS99 client needs roughly **1.5 GB resident**, before Android. That is
the floor a perfect discard implementation would leave you at:

| host RAM | per instance | instances |
|---|---|---|
| 32 GB, today (host pays full `-m`) | 2.2-3.2 GB | **10-14** |
| 32 GB, with this patch (host pays the live set) | ~1.8-2.0 GB | **~15** |
| 32 GB, Linux with balloon + free-page-reporting + KSM | ~1.6-1.8 GB | ~17 |

**30 instances of PS99 on 32 GB is not reachable on any of those rows**, and
the reason is the game rather than the hypervisor. The ~400 MB figure this
target was set from is the login screen — the number in `FOOTPRINT.md` and
in every earlier measurement here was taken against a client that had not
loaded a place. 30 instances needs a game whose in-world working set is
~700 MB, which is a per-GAME property: measure the intended place with
`omnidroid measure` before promising a fleet size for it.
