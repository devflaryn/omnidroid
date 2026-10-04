# Rooted Device R2 — P2+P3: Root & Emulator Hiding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the stock Roblox APK join PS99 (place 8737899170) and stay un-kicked — hide root from DenyList apps (defeats `AndroidRootedKick`) and spoof the device so it reads as a real phone (defeats `AndroidEmulatorKick` / the `Disconnect reason 305`), all engine-native and per app process.

**Architecture:** One per-process decision — `root::ProcessView { hidden, spoofed }` — computed once in `Process::spawn_as` from the host-only `Profile` and the app's package. A hidden process gets no root layer, no Magisk props, `omni_root`→ENOSYS, and filtered `/proc` maps/mounts. A spoofed process (`emu-hide` enabled) gets a real-phone prop set and `/proc/cpuinfo`. Both are engine seams R1 already made per-process (VFS, props, procfs); no Zygisk host (P1) is needed.

**Tech Stack:** Rust (crate `omni-linux`, `omnidroid`); the R1 `root` module (`Profile`, `Layer`, `omni_root`); per-process `PropertyService` and `procfs`.

**Spec:** `docs/superpowers/specs/2026-10-04-magisk-root-r2-design.md` (this plan implements **P2 + P3**; P1 the Zygisk host and P4 the Frida gadget are later plans).

## Global Constraints

- **Branch:** `feat/magisk-zygisk` off `feat/magisk-root` (R1). Do not merge to `main` or `perf/warm-join` in R2. Create it at plan start: `git switch -c feat/magisk-zygisk`.
- **A non-rooted device is byte-identical to R1.** When `<instance>/.omni-root-profile` is absent, `ProcessView` is `{hidden:false, spoofed:false}`, and every hide/spoof branch is a no-op. Prove it stays true in each task.
- **An un-hidden, un-spoofed process on a rooted device is unchanged from R1** (root still works for non-DenyList apps; props/proc unchanged unless `emu-hide`/denylist apply).
- **The trust root stays host-only** (`<instance>/.omni-root-profile`, read by `Profile::of`). A guest must not be able to turn hiding/spoofing on or off from any `/data` path.
- **`hidden` must fail closed:** if the package can't be determined, a DenyList/whitelist decision errs toward hiding when the profile asks for it (never accidentally reveal root to a listed app).
- **std + existing deps.** No OS-specific crates outside `omni-platform`. YAGNI: no Zygisk host, no `.so` injection, no Frida here.
- **Commit style:** small, frequent; every commit body ends with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Measure in PS99, place `8737899170`** (joined directly). The acceptance signal is the absence of `Disconnect reason received: 305` in the session log for a sustained in-world window.

## Review Focus

Inputs the spec implies that a single task's happy path won't catch — each pinned by a test in the owning task:

- **A process whose name isn't the bare package** (e.g. `com.roblox.client:gl`, or system_server): the DenyList decision must still match the base package, and a non-app process (no `--nice-name`) must not crash the view computation. Pinned in Task 1 (`view_matches_base_package_and_tolerates_missing_name`).
- **`emu-hide` enabled but a DIFFERENT app than Roblox** (scoped spoofing): a process not in scope must read the real omnidroid props unchanged, and a scoped one the Pixel props — no global leak. Pinned in Task 5 (`spoof_is_per_process_and_scoped`).
- **A hidden process that still tries `su`/`omni_root`**: a planted `su` or a raw `omni_root` svc from a DenyList app must get nothing (ENOSYS), even though the device is rooted. Pinned in Task 2 (`hidden_process_omni_root_is_enosys`).
- **Whitelist (Shamiko) mode with an empty allow list**: every app is hidden; the su-allowed uids (0/2000) still work. Pinned in Task 4 (`whitelist_hides_all_but_allowed`).
- **A spoof prop that the image sets as `ro.*` already** (e.g. `ro.product.model`): overriding it must actually take effect in the per-process area (an existing `ro.*` can't be re-`set`), so the override must go through the build-time path, not a runtime `set`. Pinned in Task 5 (`spoof_overrides_existing_ro_props`).

---

## Task 1: The per-process view seam

**Files:**
- Modify: `crates/omni-linux/src/root/profile.rs` (fill `Profile::hidden(package)`; add `Profile::spoofed(package)` and a `ProcessView`)
- Create: `crates/omni-linux/src/root/view.rs` (`ProcessView` + its computation)
- Modify: `crates/omni-linux/src/root/mod.rs` (`pub mod view;` + re-exports)
- Modify: `crates/omni-linux/src/zygote.rs` (`host_command` ~:259 — also forward `--package-name=<pkg>` when known, so the base package is reliable, not just the nice-name)
- Modify: `crates/omni-linux/src/bin/omni-linux-run.rs` (parse `--package-name` into the env/argv the process keeps; it is already after `--`, so no new parse is strictly needed if read from argv — see Step 3)
- Test: `crates/omni-linux/src/root/view.rs` and `crates/omni-linux/src/root/profile.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `Profile` (`module_ids`, `denylist`, `shamiko`, `su` — all from R1); `Profile::of(instance) -> Option<Arc<Profile>>`.
- Produces:
  - `struct ProcessView { pub hidden: bool, pub spoofed: bool }`
  - `fn ProcessView::for_process(profile: Option<&Profile>, package: Option<&str>, uid: u32) -> ProcessView`
  - `fn Profile::hidden(&self, package: Option<&str>) -> bool` (denylist contains the base package, OR shamiko is `On` and base package on denylist, OR shamiko is `Whitelist` and the package is not su-allowed-by-whitelist) — see truth table below.
  - `fn Profile::spoofed(&self, package: Option<&str>) -> bool` (`module_ids` contains `"emu-hide"`; later a scoped form `emu-hide:<pkg>,<pkg>` restricts it — in this task, unscoped `emu-hide` spoofs every app)
  - `fn base_package(name: &str) -> &str` (strips a `:suffix` process-name qualifier)

Truth table for `hidden`: denylist membership OR (`Shamiko::Whitelist` AND package not on an allow list AND uid not 0/2000). Plain `Shamiko::On` only changes P1 injection behavior (modules stay injected) — for the engine-native file/prop/proc hides it behaves like DenyList, so `hidden` is true for a denylisted package under both `On` and `Off`.

- [ ] **Step 1: Write the failing tests**

In `profile.rs`:
```rust
#[test]
fn hidden_and_spoofed_truth_table() {
    let p = Profile::parse("root=1\ndenylist=com.roblox.client\nmodule=emu-hide\nshamiko=on\n");
    assert!(p.hidden(Some("com.roblox.client")));
    assert!(p.hidden(Some("com.roblox.client:gl")));   // base package matches
    assert!(!p.hidden(Some("com.other.app")));
    assert!(!p.hidden(None));                            // unknown name, not on denylist -> not hidden
    assert!(p.spoofed(Some("com.roblox.client")));       // emu-hide unscoped spoofs all
    assert!(p.spoofed(Some("com.other.app")));

    let w = Profile::parse("root=1\nshamiko=whitelist\nsu=com.allowed\n");
    assert!(w.hidden(Some("com.anything")));             // whitelist: all hidden...
    // (su-allowed uids 0/2000 handled at the view level, not here)
}
```
In `view.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn view_matches_base_package_and_tolerates_missing_name() {
        let p = crate::root::Profile::parse("root=1\ndenylist=com.roblox.client\nmodule=emu-hide\n");
        let v = ProcessView::for_process(Some(&p), Some("com.roblox.client:gl"), 10234);
        assert!(v.hidden && v.spoofed);
        // a process with no name (system_server, a daemon) on this profile: not denylisted -> not hidden
        let v2 = ProcessView::for_process(Some(&p), None, 1000);
        assert!(!v2.hidden);
        // no profile at all -> inert
        let v3 = ProcessView::for_process(None, Some("com.roblox.client"), 10234);
        assert!(!v3.hidden && !v3.spoofed);
    }
    #[test]
    fn whitelist_allows_su_uids() {
        let p = crate::root::Profile::parse("root=1\nshamiko=whitelist\n");
        assert!(ProcessView::for_process(Some(&p), Some("com.x"), 10234).hidden);
        assert!(!ProcessView::for_process(Some(&p), Some("com.x"), 0).hidden);    // root never hidden from itself
        assert!(!ProcessView::for_process(Some(&p), Some("com.x"), 2000).hidden); // shell
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p omni-linux --lib root::view root::profile::tests::hidden`
Expected: FAIL — `ProcessView`/`hidden`/`spoofed` not found.

- [ ] **Step 3: Write minimal implementation**

`view.rs`: `ProcessView::for_process` returns `{hidden:false, spoofed:false}` when `profile` is `None`; else `hidden = uid != 0 && uid != 2000 && profile.hidden(package)`, `spoofed = profile.spoofed(package)`. `profile.rs`: `base_package(name)` = `name.split(':').next().unwrap_or(name)`; `hidden(package)` = `package.map_or(false, |n| { let b = base_package(n); self.denylist.iter().any(|d| d == b) || matches!(self.shamiko, Shamiko::Whitelist) })` — the whitelist's uid exemption is applied in the view (Step: the view already gates on uid 0/2000). For a real allow list in whitelist mode, treat `SuPolicy::Packages` as the allow list: `Shamiko::Whitelist` hides a package unless it's in `self.su` packages. `spoofed(package)` = `self.module_ids.iter().any(|m| m == "emu-hide")`. In `zygote.rs host_command` (~:259, beside the `--nice-name` arg) add `if let Some(pkg) = package { cmd.arg(format!("--package-name={pkg}")); }` (the package is in scope there, `zygote.rs:137,172`). In `omni-linux-run.rs`, the app argv already carries `--package-name` after `--`; expose a helper `fn package_of(argv) -> Option<String>` that reads `--package-name=` first, then falls back to `--nice-name=` base.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p omni-linux --lib root::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/view.rs crates/omni-linux/src/root/profile.rs crates/omni-linux/src/root/mod.rs crates/omni-linux/src/zygote.rs crates/omni-linux/src/bin/omni-linux-run.rs
git commit -m "feat(hiding): the per-process ProcessView (hidden/spoofed) from the host-only profile"
```

---

## Task 2: DenyList — hide su/modules and gate omni_root

**Files:**
- Modify: `crates/omni-linux/src/process.rs` (`spawn_as` ~:638 — compute `ProcessView` from `config.argv`'s package + `Profile::of(instance)`; pass `with_root_layer(None)` when `view.hidden`; store the view on the `Process` for procfs/syscall to read — add a `pub view: root::ProcessView` field set in `assemble`/`assemble_as`)
- Modify: `crates/omni-linux/src/root/syscall.rs` (the `omni_root` handler: return `ENOSYS` when `p.view.hidden`, before the elevate/status/setprop logic)
- Test: `crates/omni-linux/tests/hiding_boot.rs` (`#[ignore]`, uses `tests/common/boot.rs`) + a unit test on the view→layer decision if feasible

**Interfaces:**
- Consumes: `ProcessView` (Task 1), `Profile::of`, `Layer::of`, `package_of`.
- Produces: `Process.view: root::ProcessView` (read by Tasks 3 and this task).

- [ ] **Step 1: Write the failing test**

`tests/hiding_boot.rs`:
```rust
mod common;
use std::time::Duration;
#[test]
#[ignore = "boots the system; a denylisted shell sees no root"]
fn a_denylisted_process_sees_no_root() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = /* fresh temp dir */;
    // stage rooted with denylist=com.denytest (reuse root::install::stage; the --then shell below
    // runs as com.denytest by setting its nice-name/package via the control channel or a wrapper).
    stage_rooted_with_denylist(&instance, "com.denytest");
    // Easiest: run two --then checks, one as a normal context (root present) and assert su works,
    // then a process launched with --package-name=com.denytest asserts su is ABSENT and omni_root ENOSYS.
    // Marker protocol as in root_boot.rs (RB_* lines).
    // ... assert: denylisted -> `command -v su` = none, a crafted omni_root svc returns ENOSYS;
    //     non-denylisted -> su -c id -u = 0.
}
```
(If driving a named guest process through `Boot` is awkward, add a tiny harness that starts a `--then` shell with an injected `--package-name`; document it. The unit-level alternative is to assert `ProcessView::for_process` → `hidden` → the `spawn_as` branch chooses `with_root_layer(None)`; a focused test can construct a `Vfs` path without a full boot.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --test hiding_boot -- --ignored --nocapture`
Expected: FAIL — hidden process still sees su / `omni_root` still elevates.

- [ ] **Step 3: Write the implementation**

In `spawn_as` (:638): `let package = crate::root::package_of(&config.argv); let view = crate::root::ProcessView::for_process(crate::root::Profile::of(&config.instance_dir).as_deref(), package.as_deref(), uid);` then `.with_root_layer(if view.hidden { None } else { crate::root::Layer::of(&config.instance_dir) })`. Thread `view` into `assemble`/`assemble_as` and store on `Process`. In `root/syscall.rs`, first line of the handler: `if p.view.hidden { return Err(ENOSYS); }`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --test hiding_boot -- --ignored --nocapture` and `cargo test -p omni-linux --lib`
Expected: PASS; fast suite green.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/process.rs crates/omni-linux/src/root/syscall.rs crates/omni-linux/tests/hiding_boot.rs
git commit -m "feat(hiding): a denylisted process gets no root layer and omni_root returns ENOSYS"
```

---

## Task 3: DenyList — hide Magisk props and filter /proc maps & mounts

**Files:**
- Modify: `crates/omni-linux/src/props.rs` (`PropertyService::build` ~:419 — accept the process view; when hidden, do not add any root/Magisk props and ensure the layer-provided prop names are absent; when the magisk props were never added this is a no-op, but assert the invariant)
- Modify: `crates/omni-linux/src/procfs.rs` (`maps` ~:99 — skip mappings whose name matches su/magisk/module/zygisk for a hidden process; `mounts` ~:220 — skip bind lines that reveal `/data/adb`/module mounts for a hidden process)
- Test: `crates/omni-linux/src/procfs.rs` (`#[cfg(test)]`) + extend `tests/hiding_boot.rs`

**Interfaces:**
- Consumes: `Process.view` (Task 2).
- Produces: hiding in `maps`/`mounts`; a `PropertyService` built without Magisk props for a hidden process.

- [ ] **Step 1: Write the failing test**

In `procfs.rs`:
```rust
#[test]
fn hidden_maps_and_mounts_drop_root_names() {
    // a maps line naming "/system/bin/su" or a module .so or "/data/adb/..." is filtered when hidden;
    // the same line is kept when not hidden. (Call the pure line-filter fn, not a full Process.)
    assert!(!hidden_keeps("/debug_ramdisk/magisk"));
    assert!(!hidden_keeps("/data/adb/modules/x/system/lib/foo.so"));
    assert!(hidden_keeps("/system/lib64/libc.so"));
}
```
(Factor the name test into `fn is_root_mapping_name(name: &[u8]) -> bool` so it is unit-testable; `maps`/`mounts` call it only when `self.view.hidden`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --lib procfs::tests::hidden_maps`
Expected: FAIL — `is_root_mapping_name` not found.

- [ ] **Step 3: Write the implementation**

`is_root_mapping_name` matches `/debug_ramdisk`, `/data/adb`, `.../su`, a path under a module dir, and the Zygisk host name (future). In `maps` (:99-105), when `self.view.hidden`, skip entries whose name matches. In `mounts` (:220), when hidden, skip bind lines whose target/source is under `/data/adb` or a module path. In `props.rs::build`, pass the view and skip any Magisk prop addition when hidden (today none are added, so this is a guard + a test that a hidden process's `getprop` shows no `ro.*magisk*`/root markers).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --lib` and extend `hiding_boot.rs` to assert a denylisted process's `cat /proc/self/maps` has no su/module names.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/props.rs crates/omni-linux/src/procfs.rs crates/omni-linux/tests/hiding_boot.rs
git commit -m "feat(hiding): filter su/module names from a denylisted process's /proc maps and mounts"
```

---

## Task 4: Shamiko mode, whitelist, and live DenyList edit

**Files:**
- Modify: `crates/omni-linux/src/root/profile.rs` (whitelist allow-list semantics; already parsed `shamiko`)
- Modify: `crates/omni-linux/src/root/syscall.rs` (an `omni_root` op, or reuse SETPROP-style, for `magisk --denylist add|rm|ls` editing the host-only profile — uid 0 only)
- Modify: `crates/omni-linux/device/src/root/magisk.c` (wire `magisk --denylist add|rm|ls <pkg>` to the op; currently prints "unsupported") + rebuild + re-pin the binary
- Test: `crates/omni-linux/src/root/profile.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `ProcessView`, the host-only profile path (`Profile` serialize).
- Produces: `fn Profile::denylist_add/remove`, serialize back to `<instance>/.omni-root-profile`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn whitelist_hides_all_but_allowed() {
    let p = Profile::parse("root=1\nshamiko=whitelist\nsu=com.allowed\n");
    assert!(p.hidden(Some("com.other")));      // hidden
    assert!(!p.hidden(Some("com.allowed")));   // on the allow list -> visible
}
#[test]
fn denylist_add_remove_round_trips() {
    let mut p = Profile::parse("root=1\n");
    p.denylist_add("com.x");
    assert!(p.denylist.iter().any(|d| d == "com.x"));
    assert!(Profile::parse(&p.serialize()).denylist.iter().any(|d| d == "com.x"));
    p.denylist_remove("com.x");
    assert!(!Profile::parse(&p.serialize()).denylist.iter().any(|d| d == "com.x"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --lib root::profile::tests::whitelist root::profile::tests::denylist_add`
Expected: FAIL.

- [ ] **Step 3: Write the implementation**

Whitelist: in `hidden`, `Shamiko::Whitelist` hides unless `base_package` is in the `SuPolicy::Packages` allow list. `denylist_add/remove` mutate `self.denylist`. The `omni_root` op (uid 0) reads the package arg, edits `Profile::of`'s backing file (host-only), and rewrites it; `Layer::of`/`Profile::of` pick it up on the mtime reload. `magisk.c`: `--denylist add|rm|ls` issues the op; rebuild the binary (NDK, as in R1 Task 6), re-pin sha in `device/SHA256SUMS`. (Shamiko's "keep Zygisk modules injected while hidden" is a P1 concern — note in the commit that in P2, with no Zygisk host, Shamiko behaves as DenyList for the engine-native hides; whitelist is the only added behavior.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --lib root::` and verify the rebuilt binary is a valid arm64 ELF whose sha matches `device/SHA256SUMS`.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/profile.rs crates/omni-linux/src/root/syscall.rs crates/omni-linux/device/src/root/magisk.c crates/omni-linux/device/root/magisk crates/omni-linux/device/SHA256SUMS
git commit -m "feat(hiding): Shamiko whitelist mode and live magisk --denylist editing"
```

---

## Task 5: emu-hide — the device spoof prop set

**Files:**
- Create: `crates/omni-linux/src/root/spoof.rs` (the Pixel spoof profile: the prop overrides + removals)
- Modify: `crates/omni-linux/src/root/mod.rs` (`pub mod spoof;`)
- Modify: `crates/omni-linux/src/props.rs` (`PropertyService::build` — when `view.spoofed`, apply the spoof overrides as a build-time overlay BEFORE the area is frozen, so `ro.*` values actually change; remove the `ro.hardware=omnidroid` tell)
- Test: `crates/omni-linux/src/root/spoof.rs` and `crates/omni-linux/src/props.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `Process.view.spoofed`.
- Produces: `fn spoof::pixel_overrides() -> &'static [(&'static str, &'static str)]` and `fn spoof::removals() -> &'static [&'static str]`; `PropertyService::build` applies them when spoofed.

- [ ] **Step 1: Write the failing test**

```rust
// spoof.rs
#[test]
fn pixel_overrides_cover_the_emulator_tells() {
    let ov: std::collections::HashMap<_,_> = super::pixel_overrides().iter().copied().collect();
    for k in ["ro.product.model","ro.product.brand","ro.product.manufacturer","ro.product.device",
              "ro.build.fingerprint","ro.bootloader","ro.build.characteristics","ro.hardware"] {
        assert!(ov.contains_key(k), "spoof missing {k}");
    }
    assert_ne!(ov["ro.hardware"], &"omnidroid");         // the R1 tell is overridden
    assert_eq!(ov["ro.build.characteristics"], &"nosdcard"); // not "emulator"
}
// props.rs
#[test]
fn spoof_overrides_existing_ro_props_at_build_time() {
    let svc = PropertyService::for_test_spoofed(&test_sysroot()); // builds with spoof applied
    assert_eq!(svc.get("ro.product.model").as_deref(), Some("Pixel 8"));
    assert_eq!(svc.get("ro.hardware").as_deref(), Some("zuma")); // not omnidroid
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --lib root::spoof props::tests::spoof`
Expected: FAIL.

- [ ] **Step 3: Write the implementation**

`spoof.rs`: a `pixel_overrides()` table for a real Pixel 8 (brand `google`, manufacturer `Google`, model `Pixel 8`, device `shiba`, name `shiba`, `ro.hardware=zuma`, a real `ro.build.fingerprint`, `ro.bootloader`, `ro.build.characteristics=nosdcard`, `ro.product.cpu.abilist` as-is), and `removals()` for any `ro.kernel.qemu*`/`ro.boot.qemu*` keys (defensive — R1 sets none). `PropertyService::build`: take the view; when spoofed, override these keys in the `Properties` set BEFORE building the area (the override must precede the freeze because an existing `ro.*` can't be re-`set` at runtime, `props.rs:519`). Add `for_test_spoofed` mirroring R1 Task 3's `for_test`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --lib root:: props::`
Expected: PASS. Add `spoof_is_per_process_and_scoped`: a spoofed build has Pixel props; an un-spoofed build (same sysroot) has the original omnidroid props — proving per-process isolation.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/spoof.rs crates/omni-linux/src/root/mod.rs crates/omni-linux/src/props.rs
git commit -m "feat(emu-hide): per-process Pixel prop spoof applied at build time"
```

---

## Task 6: emu-hide — /proc and residual tells

**Files:**
- Modify: `crates/omni-linux/src/procfs.rs` (`cpuinfo` ~:231 and `version` ~:497 — for a spoofed process, present a plausible real-SoC `cpuinfo` and a non-"omnidroid" kernel `version`)
- Modify: `crates/omni-linux/src/root/spoof.rs` (the cpuinfo/version text)
- Test: `crates/omni-linux/src/procfs.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn spoofed_cpuinfo_and_version_have_no_tells() {
    let c = super::spoofed_cpuinfo();
    assert!(!c.contains("omnidroid") && !c.to_lowercase().contains("goldfish") && !c.to_lowercase().contains("ranchu"));
    assert!(c.contains("Hardware")); // a real /proc/cpuinfo has a Hardware line
    let v = super::spoofed_version();
    assert!(!v.contains("omnidroid"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --lib procfs::tests::spoofed_cpuinfo`
Expected: FAIL.

- [ ] **Step 3: Write the implementation**

Add `spoof::spoofed_cpuinfo()`/`spoofed_version()` (a Tensor-G3/"zuma"-style cpuinfo with a `Hardware :` line and no goldfish/omnidroid; a generic Android kernel version string). In `procfs.rs`, `cpuinfo`/`version` return the spoofed text when `self.view.spoofed`, else the current text.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/procfs.rs crates/omni-linux/src/root/spoof.rs
git commit -m "feat(emu-hide): spoof /proc/cpuinfo and /proc/version for a spoofed process"
```

---

## Task 7: The Roblox probe and the headline un-kicked boot test

**Files:**
- Create: `crates/omni-linux/tests/roblox_unkicked.rs` (`#[ignore]`, the acceptance test)
- Modify: `crates/omni-linux/src/root/spoof.rs` / `procfs.rs` / `props.rs` as the probe findings require (iterate)
- Create: `docs/research/roblox-kick-probe-2026-10-04.md` (what feeds AndroidEmulatorKick/AndroidRootedKick, found live)

**This task is empirical** — the spec fixes the mechanism (per-process spoof/hide), not the exact value set. The deliverable is the acceptance test passing; getting there is a measure-spoof-remeasure loop.

- [ ] **Step 1: Probe what Roblox checks.** Launch the stock Roblox APK rooted (`--root`, Roblox on the denylist, `--module emu-hide`) and watch the session log + a `logcat`/`dumpsys` for what it reads before the 305: use the R1 frida/lab tooling on `libroblox.so` (the omnidroid-frida skill, `lab_load`/`intercept`) to see which props/files/syscalls `AndroidEmulatorKick` and `AndroidRootedKick` consult (`docs/research/apk-analysis.md:272` is the starting point). Record findings in the research doc.

- [ ] **Step 2: Close the gaps.** For each tell the probe finds that Tasks 1-6 don't yet cover (e.g. a specific prop, a `/proc` file, a `PackageManager` feature, a `Settings.Secure` value like `adb_enabled`/`development_settings_enabled`, a `Build.TAGS`/`Build.TYPE`), add the spoof to `spoof.rs`/`props.rs`/`procfs.rs` with a focused unit test, in the same TDD style as Tasks 5-6. Add one commit per closed gap.

- [ ] **Step 3: Write the acceptance test**

```rust
#[test]
#[ignore = "boots the stock Roblox APK rooted+hidden and joins PS99; minutes"]
fn stock_roblox_joins_ps99_without_a_305_kick() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(apk) = std::env::var_os("OMNI_ROBLOX_APK") else { return }; // owner points at the stock APK
    let Some(cookie) = std::env::var_os("OMNI_ROBLOX_COOKIE") else { return };
    // stage rooted: denylist=com.roblox.client, module=emu-hide,shamiko
    // boot, plant the cookie, open place 8737899170, watch >= 5 min of in-world time.
    // ASSERT: the log shows "onGameLoaded ... placeId:8737899170"
    //     AND no "Disconnect reason received: 305" for >= 300 s after onGameLoaded.
}
```

- [ ] **Step 4: Run it and iterate.** `OMNI_ROBLOX_APK=<stock apk> OMNI_ROBLOX_COOKIE=<file> cargo test -p omni-linux --test roblox_unkicked -- --ignored --nocapture`. If a 305 still lands, go back to Step 1 (the probe tells which check fired), close the gap, repeat. Stop when the session holds ≥5 min with no 305.

- [ ] **Step 5: Commit the acceptance test + research**

```bash
git add crates/omni-linux/tests/roblox_unkicked.rs docs/research/roblox-kick-probe-2026-10-04.md
git commit -m "test(emu-hide): stock Roblox joins PS99 with no 305 kick (root+emulator hidden)"
```

---

## Task 8: CLI/MCP surface, validate, and cross-host

**Files:**
- Modify: `crates/omnidroid/src/main.rs` (ensure `emu-hide` and `shamiko` are recognized built-in module ids by `validate_modules` so `--module emu-hide,shamiko` passes without a module dir; `--root` not required when `--module` is given, as R1)
- Modify: `crates/omni-linux/tests/r_roblox.rs` (the rooted stage already carries the module list + denylist; confirm a denylist can be set from the CLI/env — add `--denylist <pkg,pkg>` to the CLI + `OMNI_R_DENYLIST`, flowing into the staged profile)
- Create: `docs/NIGHT-2026-10-04-r2-hiding.md` (R2 P2+P3 summary + the cross-host record, PENDING for the owner)
- Test: `crates/omnidroid/src/main.rs` (`parse_aosp`/`validate_modules`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn builtin_modules_and_denylist_parse() {
    let o = parse_aosp(["--module","emu-hide,shamiko","--denylist","com.roblox.client"].iter().map(|s| s.to_string())).unwrap();
    assert!(o.root && o.modules == vec!["emu-hide","shamiko"] && o.denylist == vec!["com.roblox.client"]);
    // validate_modules accepts the built-in ids with no catalog entry
    assert!(validate_modules(&["emu-hide".into(),"shamiko".into()], &empty_catalog()).is_ok());
    assert!(validate_modules(&["ghost".into()], &empty_catalog()).is_err());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p omnidroid builtin_modules_and_denylist`
Expected: FAIL.

- [ ] **Step 3: Implement.** Add a `BUILTIN_MODULES = ["emu-hide","shamiko","zygisk-frida"]` set that `validate_modules` treats as always-valid; add `--denylist`/`OMNI_R_DENYLIST` to `AospOptions`/`aosp_env`; flow it into the staged `Profile` in `r_roblox.rs` (and `root_hash` so a denylist change keys a new device).

- [ ] **Step 4: Run tests.** `cargo test -p omnidroid && cargo test -p omni-linux --test r_roblox && cargo build -p omni-mcp`. Then cross-host per [[cross-host-game-check]]: sync Mac + Linux, run `hiding_boot` + `roblox_unkicked` on each, record in the NIGHT doc.

- [ ] **Step 5: Commit**

```bash
git add crates/omnidroid/src/main.rs crates/omni-linux/tests/r_roblox.rs docs/NIGHT-2026-10-04-r2-hiding.md
git commit -m "feat(hiding): --module emu-hide/shamiko + --denylist CLI surface; R2 P2+P3 cross-host record"
```

---

## Notes for the executor

- **The view is computed once, in `spawn_as`, and read everywhere.** Don't recompute per syscall. `Process.view` is the single source.
- **Props must be spoofed at build time, not with `set`** — an existing `ro.*` can't be re-`set` (`props.rs:519`). Override the `Properties` set before the area is frozen in `PropertyService::build`.
- **Task 7 is the real work and is iterative.** Tasks 1-6 build the machine; Task 7 aims it at Roblox's actual checks. Budget for several probe→spoof→remeasure cycles; each found check is a small, tested addition.
- **Keep non-rooted and non-hidden/non-spoofed paths inert** — every branch is gated on `view.hidden`/`view.spoofed`, both false without a profile. Re-run the R1 boot tests (`root_boot`) to confirm R1 still passes.
- **`shamiko` in P2 is whitelist + DenyList-equivalent hiding.** Its "keep Zygisk modules injected" meaning only applies once P1 (the Zygisk host) exists.
