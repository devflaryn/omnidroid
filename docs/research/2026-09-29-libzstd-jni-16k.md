# What `libzstd-jni` does on a 16 KiB guest -- and on a 4 KiB one (Task 0 of the 4 KiB guest plan)

Date: 2026-09-29. Library: `lib/arm64-v8a/libzstd-jni-1.5.7-6.so` from Roblox 2.740.931 (Delta
build), 18,434,968 bytes. Time-boxed; the mechanism is **not** fully pinned (why at the end).

## Measured

**Program headers (from the APK, unmodified):**

| segment | flags | `p_offset` | `p_vaddr` | `p_vaddr - p_offset` | `p_filesz` | `p_memsz` | `p_align` |
|---|---|---|---|---|---|---|---|
| LOAD (text) | R-X | `0x0` | `0x0` | 0 | `0x10fbf00` | same | `0x1000` |
| LOAD (relro data) | RW- | `0x10fbf00` | `0x10fcf00` | `0x1000` | `0x8d050` | same | `0x1000` |
| LOAD (data, bss) | RW- | `0x1188f50` | `0x118af50` | `0x2000` | `0xb440` | `0x1a1f8` | `0x1000` |
| GNU_RELRO | R-- | `0x10fbf00` | `0x10fcf00` | | `0x8d050` | `0x8d100` | |

* The data segments' address and offset agree modulo 4 KiB but **not** modulo 16 KiB. A 16 KiB
  `linker64` cannot map them as they are, which is why `pagecompat` rewrote the file on the Mac: it
  moved every segment to its address plus one constant (a new file of 18,443,160 bytes) and trimmed
  `PT_GNU_RELRO`.
* The relro end (`0x118a000`) and the start of `.data` (`0x118af50`) share one 16 KiB page
  (`[0x1188000, 0x118c000)`). The 16 KiB guest could only either seal `.data` with relro or leave
  the tail of relro writable; `pagecompat` chose the second.
* The text segment is 17 MB, and its code is ciphertext until a constructor decrypts it. The
  decryption `mprotect`s 4 KiB pieces, which is what `protect_widened` was written for (`9f5632b`,
  `d2f0852`).

**The crash** (`$TMPDIR/omni-linux-r-69661.log`, the previous session's run on the 16 KiB guest):
* `Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x40`, on a thread of the app
  process, about a minute after launch, after `preload_zstd_dictionary_end`.
* The return address is `libzstd-jni-1.5.7-6.so+0x5f42a0`, inside the (re-laid-out) text segment.
* `x9 = 0x40`: a null base plus 0x40, a slot of a table whose pointer was never written.
* The faulting pc itself was not recorded (no signal trace in that run). The plaintext dumps made
  then (`~/omni-dump/...`) no longer exist.

**The faulting instruction** (captured 17:20 the same day, on a quiet host,
`OMNI_SIGNAL_TRACE_APP=com.roblox.client`, `$TMPDIR/omni-linux-r-10946.log:27647`). The same crash
happened 20 s after the library loaded, with `fault addr 0x40` at pc `libzstd-jni+0x5f4320`, lr
`+0x5f42a0` and frame `#00 +0x63d478`. Disassembled from the trace's code dump:

```
+0x5f42d0  ldr  x9, [x9]          ; a pointer loaded from one of the library's globals
  ...      (mixed boolean arithmetic: x8 = *global + 0x3f, obfuscated)
+0x5f4320  ldur x8, [x8, #1]      ; *(*global + 0x40)  <- faults: *global is null
+0x5f4324  blr  x8                ; an indirect call through that table slot
```

So the obfuscated (OLLVM-style) library calls through slot 0x40 of a function table whose pointer,
in its own writable data, is still null on this thread's first use. The table is filled by the
library's own initialisation after it decrypts itself; on the 16 KiB guest that initialisation
did not fill it.

## What a 4 KiB guest changes

* The file is no longer rewritten: `linker64` maps each segment where its headers say, at 4 KiB
  granularity (a private copy for the data segments, whose offsets are not congruent with the host
  page).
* Relro seals exactly `[0x10fcf00 & ~0xfff, 0x118a000)`, and `.data` stays writable from
  `0x118a000`.
* The unpacker's 4 KiB `mprotect`s are exact (no union of neighbours' protections).
* `getpagesize()` is 4096.

## Hypotheses, not established

1. **The re-laid-out file.** A packed library that reads its own file from disk -- to fetch
   ciphertext or check its integrity -- at offsets fixed when it was packed reads the wrong bytes
   from a file `pagecompat` moved. It then skips building a table it would otherwise fill.
   Consistent with a *never-initialised* table (null + 0x40), not a corrupted one.
2. **The protections.** A union protection left a page writable that the unpacker expects
   read-only (or the reverse), and a check took a different branch.

Both are removed by the 4 KiB guest, so the gate distinguishes "fixed" from "not fixed" without
pinning which one it was. A standalone reproduction would need the app's `JNI_OnLoad` context
(`c2_apk_in_app_process.rs` notes why a bare `app_process` is not enough).

## Why it stopped here

Four boots earlier in the day died at about 220 s, before the app launched. `system_server`'s 60 s
Watchdog fired on a host at load 21-25, with Chrome's GPU process at about 440% CPU. The fifth,
with Chrome quit, booted and launched the app, and captured the pc above. Pinning which global
stays null would take the decrypted text, which is out of scope: the fix removes both reasons the
library could see a different world.

## The 4 KiB guest does not fix it (measured 2026-09-29, 19:10)

With the 4 KiB guest built (D42, `73e6f60`) the full gate boots, installs and launches the app
(`$TMPDIR/omni-linux-r-25289.log`). `libzstd-jni` is loaded by `linker64` at 4 KiB pages from the
unmodified file (no `pagecompat`), and every 4 KiB `mprotect` is exact. The app then dies the same
way, 20 s after the library loads:
* `Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x40`;
* lr `libzstd-jni-1.5.7-6.so+0x5f42a0`, the same instruction as on the 16 KiB guest.

**Both hypotheses above are falsified.** The page size, the rewritten file and the union
protections are not the cause. What the Mac run differs in from the Windows and Linux runs that
work is elsewhere: the arm64 dynarmic backend, the low window (D41), and the host itself.

The obvious suspect for a self-decrypting, obfuscated library is **stale translated code**. Its
constructors decrypt `.text` in place and then run it. On arm64, `Open` in `docs/ports/macos.md`
records a mid-run cache-clear defect: patch 0031 is here, but `arm64-clear-audit`'s patch 0023 is
not merged. Not yet tested.
