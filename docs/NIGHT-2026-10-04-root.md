# Night 2026-10-04: Magisk-style root (R1)

## What shipped (R1)

A device can be booted rooted, with no VM and no real Magisk daemon on the host side:

- `root=1` in the host-only profile (`<instance>/.omni-root-profile`, never guest-writable) makes the
  device rooted; a guest-planted marker alone grants nothing.
- `su` from the shell or an app returns uid 0 (policy `su=all`; default is shell/root only).
- Magisk modules (zip or directory with `module.prop`) are staged, their `post-fs-data.sh` and
  `service.sh` run at boot, and their `system/` tree overlays `/system`. `resetprop` works.
- CLI `--root`, `--module <id|zip>`, `--su <all|pkg,...>`; `modules` subcommand; MCP parameters;
  a rooted golden key; warm reuse compares the full root hash.
- Magisk pinned to **v30.7** (assets in `sysroot/magisk-v30.7/`, fetched by `tools/fetch_magisk.py`).

## Tests and how to run them

All boot tests are `#[ignore]` (minutes each) and no-op without `sysroot/aosp-35`.

| Test | Command | Needs |
|---|---|---|
| rooted boot, module, resetprop, no-profile negative (Task 8) | `cargo test -p omni-linux --test root_boot -- --ignored --nocapture` | sysroot, Magisk assets |
| community module smoke | `OMNI_ROOT_SMOKE_MODULE=<module.zip> cargo test -p omni-linux --test root_boot community_module -- --ignored --nocapture` | the env var (skips when unset); reference module: MagiskHide Props Config |
| su-probe granted / refused | `cargo test -p omni-linux --test root_boot su_probe -- --ignored --nocapture` | Android SDK build-tools 36.0.0 + android-36, JDK 21, Git Bash (skips when absent) |

The su-probe app is source only: `crates/omni-linux/tests/fixtures/su-probe-app/` (build with its
`build.sh [out.apk]`; the APK is gitignored). Its activity runs `su -c id -u` and logs
`omni-su-probe uid=<n>` or `omni-su-probe refused: ...`.

Note: R1's su policy is uid-based. A `su=<pkgs>` list is stored but not yet matched to an app's
package, so any non-`all` policy refuses apps. The refused test relies on that.

Fast suite: `cargo test -p omni-linux --lib`.

## Windows results

(controller: fill in)

## Cross-host: PENDING (owner)

To run per the cross-host rule once Windows is green: sync Mac (`berat@macmini.local`) and Linux
(`berat@192.168.0.38`) to `feat/magisk-root`, run
`cargo test -p omni-linux --test root_boot -- --ignored` on each, and record here:

- Mac: PENDING
- Linux: PENDING
- Any `AndroidRootedKick` behaviour seen: PENDING
- Screenshot of a rooted `su -c id` sent to the owner: PENDING
