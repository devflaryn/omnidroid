# Boot on (nearly) every device — kill the wall-clock timeouts

**Goal (2026-08-21):** omnidroid + omni-executor should be more stable, boot
faster where that is free, and **boot on nearly every PC**. Today a slow or
unaccelerated host is reported as a failure even when the guest is booting
perfectly well, just slowly.

## The four defects this plan closes

### 1. A slow host is killed mid-boot (`engine.wait_for_boot`)
`wait_for_boot` runs against a fixed wall-clock budget —
`NORMAL_BOOT_TIMEOUT = 360`, `FIRST_BOOT_TIMEOUT = 1500`, `RESTORE_TIMEOUT = 30`.
Those numbers were measured on an i7-13700F. On a weak CPU, a spinning disk, or
a host with no hypervisor at all (see 3), a boot that is *visibly progressing*
is declared `boot_timeout` and the launch fails.

**Fix:** wait on PROGRESS, not on the clock. A new `omnidroid/bootwait.py`
samples independent progress signals each poll — serial/qemu log growth, the adb
endpoint's state, the guest's own boot properties, and QEMU's accumulated host
CPU time — and only gives up when *nothing* has moved for a stall window. An
explicit `--timeout` still caps absolutely, for scripts that need a bound; the
default becomes "no cap, stall-limited".

### 2. The app kills the engine at 660 s and orphans the VM (`omni-executor`)
`run_engine(..., timeout=)` arms `threading.Timer(timeout, proc.kill)` — an
ABSOLUTE deadline. `cmd_start` passes `BOOT_TIMEOUT + WATCHDOG_GRACE` = 660 s.
Past that the app kills the engine while QEMU keeps running detached: the UI
says the start failed and a live instance is orphaned.

**Fix:** an IDLE watchdog. The engine prints a progress line at least every 15 s
during a boot, so the watchdog is rearmed on every line and fires only after
`timeout` seconds of genuine SILENCE. A wedged engine is still caught; a slow
one is not. `start` stops passing `--timeout` so the engine's stall logic governs.

### 3. No hypervisor = no boot, with no explanation (`qemu_proc.default_accel`)
`default_accel()` returns `whpx,kernel-irqchip=off` on Windows unconditionally
and nothing ever checks it is usable. On a PC where "Windows Hypervisor
Platform" is not installed, or VT-x/AMD-V is off in BIOS, QEMU exits during
machine init and the user gets "QEMU exited before the guest booted". There is
no fallback and no advice. `check_accel()` only covers Linux.

**Fix:** `omnidroid/accelprobe.py`. Probe the candidate accelerator by starting
QEMU with `-nodefaults -display none -m 64 -S -qmp stdio` and watching for the
QMP greeting: the greeting means machine init (and therefore accel init)
succeeded, an early exit means it did not. Measured at **0.06 s** on this host,
and it correctly rejects an unavailable accelerator. Cache the verdict per
(platform, qemu path, qemu mtime) so it is paid once, fall back
`whpx -> tcg` / `kvm -> tcg` / `hvf -> tcg`, and print the exact command that
would enable the real thing. TCG is slow, but slow now boots (see 1).

### 4. Free seconds thrown away on every boot
`wait_for_boot` sleeps a flat 5 s between polls, so every boot pays up to 5 s
(2.5 s expected) of pure latency after Android is already up. `post_boot` and
`provision_settings` sleep a fixed 3 s / 2 s waiting for `adb root` to come back.

**Fix:** adaptive poll interval (fast once adbd is up, since `boot_completed` is
imminent then) and a readiness poll instead of a fixed sleep after `adb root`.

## Order of work
1. `accelprobe.py` + tests, wire into `default_accel`/`spawn_qemu`/warm key/`doctor`. ✅
2. `bootwait.py` + tests, rewire `wait_for_boot`. ✅
3. Adaptive polling + the two fixed sleeps. ✅
4. omni-executor idle watchdog + tests. ✅

## Found while doing it (not in the original four)

- **5. The warm cache was refused to the hosts that need it most.**
  `_warm_cache_allowed()` asked `default_accel()` — the platform's *preference* —
  so on Windows it always said `whpx`, which cannot migrate, and refused the
  cache. **TCG migrates fine**, and a TCG host is exactly the one whose cold
  boots are unbearable. It asks `effective_accel()` now. This is what makes the
  fallback message's promise ("the FIRST boot is the slow one") true.
- **6. The adb-offline recovery thresholds were poll counts.** `8` and `25`
  meant 40 s and 125 s at a flat 5 s poll. The adaptive poll in (3) would have
  silently rescheduled both — firing the *hard* step, a host-wide
  `adb kill-server` that drops every other instance's endpoint, 75 s into a
  healthy slow boot. Converted to seconds.
- **7. `wait_for_game_settled` had the same bug with a worse consequence.**
  `SETTLE_TIMEOUT_S = 420` then squeezed regardless — and this project has
  already measured that squeezing a loading client starves it. It does not
  report a failure; it produces an instance that farms nothing while looking
  healthy. PSS growth extends the deadline now, under `SETTLE_CEILING_S`.
- **8. Two backstops** for the failures that never go quiet, since dropping the
  deadline dropped a safety net it was providing by accident: a reboot loop
  (adbd reached and lost 4×) and `SANITY_CEILING_S` (3 h).
- **9. Self-inflicted, caught in review:** the stall warning carried a live
  elapsed time and was folded into the phase string that drives "print when the
  phase changes" — at a 1 s poll that is one line a second. Split.

- **10. THE BIG ONE, and it only surfaced because the fallback was tested for
  real: an emulated x86 guest needs `-cpu max`.** The first fallback did not
  boot at all. `-cpu qemu64,+aes` under TCG produced **0 bytes of kernel log in
  240 s** — the kernel never printed line one — against 77 KB from the same
  plumbing under WHPX. `-cpu max` gives 92 KB and `bootcomplete` in 104 s.
  `qemu64` is a K8-era baseline against a clang-LTO xanmod 6.1 kernel; WHPX only
  survives it because its CPUID filtering is limited, so the mask was never
  really being applied.
- **11. Two bugs in my own fallback, both from the same shortcut.**
  `tcg,thread=multi` is REJECTED by QEMU on x86 (`Property
  'pc-q35-11.1-machine.thread' not found` — x86 folds the accel into the
  *machine* string, where `thread` is not a machine property), and it shipped
  because TCG was being returned **unproven** as "always compiled in". The
  fallbacks are a proven candidate list now; nothing is exempt.

## Verification

- omnidroid: **13 failed / 1511 passed** — 13 of the pre-existing 15, the two
  `test_x86_cpu_model` ones being fixed on the way (they still expected
  `qemu64` after `+aes` was added). No new failures. omni-executor: **1 failed /
  251 passed**, its own pre-existing baseline. ~75 new tests.
- Accelerated, live: warm-restore launch **0.3 min**, cold `--no-warm` **0.4
  min**, both joining PS99; `doctor` reporting `accel_hardware: true`.
- **Emulated (`--accel tcg`), through the product's own path:** adbd at **1.8
  min**, `sys.boot_completed` at **2.2 min**, native bridge OK, session
  delivered, place joined. Before this work such a host could not boot at all;
  with the first fallback it spun forever.
- The no-game fast path fired correctly on that run: *"no game process after
  120s — there is nothing to settle, so not waiting out the remaining 292s"*.

## The diagnostic technique worth keeping

A guest that never starts is **indistinguishable from a very slow one** from
outside — QEMU burned 99 % of a core for 37 minutes and the progress watch
called it healthy the whole time. The tell was **zero disk I/O**
(`GetProcessIoCounters`: 21.3 MiB read in total, not one byte in a 20 s
sample); a booting kernel reads continuously. Then attach `-serial file:` and
add `console=ttyS0 loglevel=7 ignore_loglevel` to `-append` — the x86 path ships
no serial console on a normal boot, which is why this was invisible. **Run the
working accelerator as a control first:** a 0-byte log proves nothing until the
same plumbing has produced 77 KB on a boot known to work.

## Non-goals
Not touching the memory governor, the pool, or the open PS99 OOM question.
