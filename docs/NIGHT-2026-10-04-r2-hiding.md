# Night 2026-10-04: root and emulator hiding (R2 P2+P3)

## What shipped

- Per-process hiding: a process on the denylist (or, in Shamiko whitelist mode, any process not on the
  `su` allow list) gets a view with no root layer, no root in `/proc/*/maps` and mounts.
- DenyList: `magisk --denylist add|ls|rm` edits the host-only profile live (root only; a non-root
  caller is refused). Shamiko whitelist mode (`shamiko=whitelist`).
- `emu-hide`: the property set of a real Pixel 8 applied at build time (boot-state and build-key
  consistency, qemu props removed by prefix), plus a spoofed `/proc/cpuinfo` and `/proc/version`.
- The pinned-Pixel profile: one Pixel 8 identity used for props, cpuinfo and version.
- CLI (Task 8): `--module emu-hide,shamiko` (engine-native ids, valid with no catalog entry:
  `emu-hide`, `shamiko`, `zygisk-frida`), `--denylist com.roblox.client` (implies `--root`),
  `OMNI_R_DENYLIST` in the session environment. The denylist is in the staged profile text, so it is in
  the root hash and a denylist change is a different saved/warm device. Unrooted launches are unchanged.
- Semantics: `--module shamiko` = whitelist mode (hides root from EVERY app not in --su; use --denylist for a specific app). `--module zygisk-frida` is accepted but a NO-OP until the P1 Zygisk host exists.

## Tests and how to run them

| Test | Command | Needs |
|---|---|---|
| CLI parse, built-ins, denylist | `cargo test -p omnidroid builtin_modules_and_denylist` | nothing |
| golden key incl. denylist | `cargo test -p omni-linux --test r_roblox golden_key` | nothing |
| hiding boot | `cargo test -p omni-linux --test hiding_boot -- --ignored --nocapture` | sysroot, Magisk assets |
| denylist op (`magisk --denylist`) | `cargo test -p omni-linux --test hiding_boot magisk_denylist -- --ignored --nocapture` | sysroot, Magisk assets |
| Roblox, not kicked (Task 7, owner-run) | `cargo test -p omni-linux --test roblox_unkicked -- --ignored --nocapture` | sysroot, owner cookie, network |

A launch: `omnidroid aosp --cookie <file> --place <id> --module emu-hide,shamiko --denylist com.roblox.client`.

## Cross-host: PENDING (owner)

Sync Mac and Linux, run `hiding_boot` and `roblox_unkicked` on each (see the cross-host game check).
Not run: owner-gated. Record results here.

## Task 7 live probe: PENDING (owner)

The live `roblox_unkicked` probe against Roblox's real checks, with the owner's account. Not run.
Record the probe/spoof/remeasure cycles here.
