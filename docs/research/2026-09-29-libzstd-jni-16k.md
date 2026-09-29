# What `libzstd-jni` does on a 16 KiB guest (Task 0 of the 4 KiB guest plan)

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

Four boots attempted today to capture the faulting pc all died at about 220 s, before the app
launched. `system_server`'s 60 s Watchdog fired on a host at load 21-25: Chrome's GPU process was
using about 440% CPU. That was a separate problem, diagnosed from the logs (boot phases 1.4-2.0x
slower than the good run, a different blocked frame each time). A quiet host is needed for the
gate itself.
