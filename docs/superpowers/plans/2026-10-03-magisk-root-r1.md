# Rooted Device R1 — Root Core & Module Picker — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make omnidroid boot as a rooted device — `su` elevates to uid 0, and standard Magisk module zips selected per launch overlay files, set props and run boot scripts — with the picker on the CLI and MCP, and with a non-rooted launch byte-for-byte unchanged.

**Architecture:** omnidroid does not run Magisk; the engine provides what Magisk provides. A per-instance **root profile** file says whether the device is rooted and which modules load. A per-instance **root layer** in the VFS (the same cached-by-instance, mtime-reloaded pattern as `Binds`) overlays the enabled modules' `system/` files and the `su`/`magisk`/`resetprop` tool binary onto `/system`. A tiny NDK-built `magisk` multi-call binary asks the engine, through one engine-private syscall, to raise the caller to uid 0. A boot service installs the module zips (via Magisk's own `util_functions.sh` + `busybox`), applies each module's `system.prop`, and runs `post-fs-data.sh`/`service.sh`.

**Tech Stack:** Rust (crate `omni-linux`, `omnidroid`, `omni-warm`, `omni-mcp`); C built with Android NDK r28c (committed, sha-pinned, as `device/src/mapper.c` already is); Python 3 for the Magisk-asset fetch tool.

**Spec:** `docs/superpowers/specs/2026-10-03-magisk-root-design.md` (this plan implements the **R1** section only; R2/R3 are later plans).

## Global Constraints

- **Branch:** `feat/magisk-root` (already created from `perf/warm-join`). Do not merge to `main` in R1.
- **No OS-specific crates outside `omni-platform`** (`windows-sys`, `libc`, …). `omni-linux` is the kernel personality; keep new code within the crate's existing dependency set (`std`, `parking_lot`, `sha2`, `omni-apk` for zip reading if added as a dep). Global Constraint 4.
- **A non-rooted launch is unchanged.** When `<instance>/data/adb/omni/profile` is absent, no root layer is attached, no tool binary exists, and `omni_root` returns `ENOSYS`. Prove this in Task 8's no-profile boot and keep it true in every task.
- **GPL binaries are never committed.** The pinned Magisk APK and the files extracted from it (`util_functions.sh`, `busybox`) live under `sysroot/magisk-<ver>/`, which is gitignored like `sysroot/aosp-35/`. Our own `magisk` multi-call binary (our source, our build) **is** committed with its sha in `device/SHA256SUMS`, exactly as `mapper.omni.so` is.
- **Private syscall number:** `OMNI_ROOT = 510` — outside the real arm64 asm-generic range, inside the 512-entry dispatch table (`syscall::Table`, `TABLE_LEN = 512`). No AOSP-35 code or bionic ever issues it; only our `magisk` binary does.
- **Magisk module zip format is the contract.** Read real community zips unchanged: `module.prop`, `system/`, `system/vendor|product|system_ext`, `.replace` dirs, `system.prop`, `post-fs-data.sh`, `service.sh`, `customize.sh`, `skip_mount`, `disable`, `remove`, `uninstall.sh`. Ignore `sepolicy.rule` (SELinux is permissive here).
- **Commit style:** small, frequent; end every commit message with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- **Toolchain:** NDK r28c at `~/android-ndk/android-ndk-r28c` (the build note in `device/src/build.txt` is the template). Python 3.11 is on PATH as `python`.

## Review Focus

Inputs the spec implies but that no single task's happy-path tests fully exercise — each is pinned by a test in the owning task:

- **A module zip whose `system/` tries to overwrite a file the image genuinely has** (e.g. `system/bin/sh`): must replace in the layer, never error the way the device overlay does for its own paths. Pinned in Task 5 (`replaces an image file`).
- **A malformed / empty / missing `module.prop`** in the catalog: must be a named error before boot, not a panic and not a silently-skipped module. Pinned in Task 2 (`rejects a module with no id`).
- **`su` requested by a uid the profile does not allow** (`su=<pkg>` and a different caller): must be `EACCES` → `su` exits 1, never a silent elevation. Pinned in Task 4 (`elevation refused for a disallowed uid`).
- **A rooted profile selecting a module id that is not in the catalog:** CLI fails before boot naming the id; it must not boot a device that silently lacks the module. Pinned in Task 9 (`unknown module id is rejected`).
- **The same instance opened by two host processes** (system_server and an app) while modules are being installed: the layer must become visible to processes started after `layer.gen` is touched, and not half-built to one started during install. Pinned in Task 8's boot test (app sees `/system/etc/omni-test.txt` after `sys.boot_completed`).

---

## Task 1: Root profile type

**Files:**
- Create: `crates/omni-linux/src/root/mod.rs`
- Create: `crates/omni-linux/src/root/profile.rs`
- Modify: `crates/omni-linux/src/lib.rs` (add `pub mod root;`)
- Test: in `crates/omni-linux/src/root/profile.rs` (`#[cfg(test)]`, the pattern `init.rs`/`owners.rs` use)

**Interfaces:**
- Produces:
  - `omni_linux::root::Profile` with:
    - `fn parse(text: &str) -> Profile`
    - `fn serialize(&self) -> String`
    - `fn is_rooted(&self) -> bool`
    - `fn su_allowed(&self, uid: u32) -> bool` (uid 0 and 2000 always true; `su=all` → all; `su=<pkgs>` is R2-resolved by package, so in R1 a package list means "only 0/2000 by uid" — all other uids false unless `all`)
    - `fn modules(&self) -> &[String]` (ids, in order)
    - `fn magisk_version_code(&self) -> u32`
    - `fn hidden(&self, _package: Option<&str>) -> bool { false }` (R2 fills this in)
  - `struct Profile { pub rooted: bool, pub magisk_code: u32, pub module_ids: Vec<String>, pub su: SuPolicy, pub denylist: Vec<String>, pub shamiko: Shamiko }`
  - `enum SuPolicy { All, Packages(Vec<String>) }`
  - `enum Shamiko { Off, On, Whitelist }`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_reads_fields() {
        let text = "root=1\nmagisk=29000\nmodule=zygisk-frida\nmodule=emu-hide\nsu=all\ndenylist=com.roblox.client\nshamiko=whitelist\n";
        let p = Profile::parse(text);
        assert!(p.is_rooted());
        assert_eq!(p.magisk_version_code(), 29000);
        assert_eq!(p.modules(), &["zygisk-frida".to_string(), "emu-hide".to_string()]);
        assert!(p.su_allowed(10234));          // su=all
        assert_eq!(p.denylist, vec!["com.roblox.client".to_string()]);
        assert!(matches!(p.shamiko, Shamiko::Whitelist));
        // serialize -> parse is stable
        assert_eq!(Profile::parse(&p.serialize()).serialize(), p.serialize());
    }

    #[test]
    fn absent_profile_is_not_rooted_and_su_is_uid_gated() {
        let p = Profile::parse("");            // empty == no root
        assert!(!p.is_rooted());
        let p = Profile::parse("root=1\nsu=com.some.pkg\n");
        assert!(p.su_allowed(0));              // root always
        assert!(p.su_allowed(2000));           // shell always
        assert!(!p.su_allowed(10234));         // a package policy denies other uids in R1
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p omni-linux --lib root::profile -- --nocapture`
Expected: FAIL — `root` module / `Profile` not found.

- [ ] **Step 3: Write minimal implementation**

`root/mod.rs`:
```rust
//! The engine-native, Magisk-compatible rooted device (docs/superpowers/specs/2026-10-03-magisk-root-design.md).
//! R1: the per-instance root profile, the module catalog, the root layer over /system, the `su`
//! tool and the `omni_root` syscall. When an instance has no profile, nothing here takes effect.
pub mod profile;
pub use profile::{Profile, Shamiko, SuPolicy};
```

`root/profile.rs`: parse line-oriented `key=value` (ignore blank lines and `#` comments); `module=` lines accumulate in order; `su=all` → `SuPolicy::All`, otherwise comma-split packages; `shamiko=1|on` → `On`, `whitelist` → `Whitelist`, else `Off`; `denylist=` comma or repeated. `is_rooted` = `rooted`. `su_allowed(uid)` = `uid == 0 || uid == 2000 || matches!(self.su, SuPolicy::All)`. `serialize` writes the canonical form the test round-trips.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p omni-linux --lib root::profile`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/ crates/omni-linux/src/lib.rs
git commit -m "feat(root): the per-instance root profile (parse, su policy, module list)"
```

---

## Task 2: Module catalog

**Files:**
- Create: `crates/omni-linux/src/root/module.rs`
- Modify: `crates/omni-linux/src/root/mod.rs` (add `pub mod module;` + re-exports)
- Modify: `crates/omni-linux/Cargo.toml` (add `omni-apk = { workspace = true }` to `[dependencies]` — the committed zip reader; it is `#![forbid(unsafe_code)]` and pure file/compute, so Global Constraint 4 is satisfied)
- Create (test fixtures): `crates/omni-linux/tests/data/mod-a/module.prop`, `crates/omni-linux/tests/data/mod-a/system/etc/a.txt`
- Test: in `crates/omni-linux/src/root/module.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `omni_apk::Apk` (`open`, `entries`, `entry`, `read_named`) for zip-form modules.
- Produces:
  - `struct ModuleProp { pub id: String, pub name: String, pub version: String, pub version_code: i64, pub author: String, pub description: String }`
  - `fn ModuleProp::parse(text: &str) -> Result<ModuleProp, String>` (error names the missing field)
  - `enum ModuleSource { Dir(PathBuf), Zip(PathBuf) }`
  - `struct Module { pub prop: ModuleProp, pub source: ModuleSource }` with `fn read(&self, rel: &str) -> Option<Vec<u8>>` and `fn has(&self, rel: &str) -> bool` and `fn list(&self, rel_dir: &str) -> Vec<String>` (entries directly under `rel_dir`), all reading a dir or a zip uniformly.
  - `struct Catalog { modules: Vec<Module> }` with:
    - `fn discover(builtin_dir: &Path, user_dir: Option<&Path>) -> Result<Catalog, String>` (user dir first, then built-in; duplicate id → `Err` naming both sources)
    - `fn find(&self, id: &str) -> Option<&Module>`
    - `fn list(&self) -> &[Module]`
  - `fn builtin_dir(repo_root: &Path) -> PathBuf` → `repo_root/modules`
  - `fn user_dir() -> Option<PathBuf>` → `$OMNI_MODULES` else `<home>/.omnidroid/modules` (home via `omni_platform`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn data(rel: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(rel) }

    #[test]
    fn parses_a_module_prop() {
        let p = ModuleProp::parse("id=mod_a\nname=Mod A\nversion=v1\nversionCode=1\nauthor=me\ndescription=x\n").unwrap();
        assert_eq!(p.id, "mod_a");
        assert_eq!(p.version_code, 1);
    }

    #[test]
    fn rejects_a_module_with_no_id() {
        assert!(ModuleProp::parse("name=No Id\n").is_err());
    }

    #[test]
    fn discovers_a_dir_module_and_reads_a_file() {
        let cat = Catalog::discover(&data(""), None).unwrap();
        let m = cat.find("mod_a").expect("mod-a present");
        assert_eq!(m.read("system/etc/a.txt").as_deref(), Some(&b"hello\n"[..]));
        assert!(m.has("system/etc/a.txt"));
        assert_eq!(m.list("system/etc"), vec!["a.txt".to_string()]);
    }
}
```
Fixture files: `tests/data/mod-a/module.prop` = `id=mod_a\nname=Mod A\nversion=v1\nversionCode=1\nauthor=me\ndescription=x\n`; `tests/data/mod-a/system/etc/a.txt` = `hello\n`. (`discover` treats each subdirectory of `builtin_dir` containing `module.prop`, and each `*.zip`, as a module.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p omni-linux --lib root::module`
Expected: FAIL — `module` not found.

- [ ] **Step 3: Write minimal implementation**

Implement `ModuleProp::parse` (split `key=value`, require `id`; default the rest to `""`/`0`). `Module::read/has/list` branch on `ModuleSource`: `Dir` → `std::fs`; `Zip` → `omni_apk::Apk::open` once (cache the `Apk` in the `Module` via `OnceLock` or re-open on read — re-open is fine for R1). `Catalog::discover` scans `user_dir` then `builtin_dir`; a second module with an id already seen → `Err(format!("module id {id} is in both {a} and {b}"))`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p omni-linux --lib root::module`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/module.rs crates/omni-linux/src/root/mod.rs crates/omni-linux/Cargo.toml crates/omni-linux/tests/data/mod-a
git commit -m "feat(root): the module catalog (module.prop, dir/zip modules, duplicate-id error)"
```

---

## Task 3: `resetprop` bypass in the property service

**Files:**
- Modify: `crates/omni-linux/src/props.rs` (add a forced setter + delete; refactor the existing `set` body so the ro.-guard is the only difference)
- Test: `crates/omni-linux/src/props.rs` (`#[cfg(test)]`, extend the existing module)

**Interfaces:**
- Produces on `PropertyService`:
  - `fn set_forced(&self, name: &str, value: &str) -> u32` — same as `set` but never returns `PROP_ERROR_READ_ONLY_PROPERTY`; a first-time `ro.*` and an overwrite of an existing `ro.*` both succeed; still publishes to every mapping exactly like `set`.
  - `fn delete(&self, name: &str) -> bool` — removes a name from `info` so `get` returns `None` (the area keeps the bytes; readers re-reading the serial see it vanish via `info`). Returns whether it existed.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn set_forced_overrides_a_read_only_property_and_delete_removes_it() {
    let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
    let svc = PropertyService::global(&root);
    // An existing ro.* cannot be changed by set...
    let name = "ro.build.tags";
    svc.set(name, "release-keys");
    assert_eq!(svc.set(name, "test-keys"), PROP_ERROR_READ_ONLY_PROPERTY);
    // ...but set_forced changes it.
    assert_eq!(svc.set_forced(name, "test-keys"), PROP_SUCCESS);
    assert_eq!(svc.get(name).as_deref(), Some("test-keys"));
    assert!(svc.delete(name));
    assert_eq!(svc.get(name), None);
}
```
(If `PropertyService::global` is a process-wide `OnceLock` that makes a second test's service collide, add a test-only `PropertyService::for_test(sysroot)` constructor that builds a fresh instance with the same `Live` initialization, and use it here. Note that in `props.rs`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p omni-linux --lib props`
Expected: FAIL — `set_forced` not found.

- [ ] **Step 3: Write minimal implementation**

Extract the current `set` body (from `let _publishing` onward) into `fn set_inner(&self, name, value, force: bool) -> u32`, changing only the ro. line: `Some(_) if name.starts_with("ro.") && !force => return PROP_ERROR_READ_ONLY_PROPERTY,`. `set` calls `set_inner(.., false)`; `set_forced` calls `set_inner(.., true)`. `delete` locks `live`, `live.info.remove(name)`, bumps serial, publishes (reuse the publish tail, or simply notify — a removed name's old bytes are harmless since `get`/`value` key off `info`). Keep `ctl.`/validation handling in `set` ahead of `set_inner`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p omni-linux --lib props`
Expected: PASS (existing test + the new one).

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/props.rs
git commit -m "feat(root): resetprop support -- PropertyService::set_forced bypasses the ro. guard"
```

---

## Task 4: The `omni_root` syscall (ELEVATE / STATUS)

**Files:**
- Modify: `crates/omni-linux/src/syscall.rs` (add `OMNI_ROOT = 510 => "omni_root"` to the `numbers!` block)
- Modify: `crates/omni-linux/src/sys.rs` (add `SysState::assume(uid, gid, groups, caps)` — set ids and caps together, for engine-granted elevation)
- Create: `crates/omni-linux/src/root/syscall.rs` (the handler + the pure elevation decision + `install`)
- Modify: `crates/omni-linux/src/root/mod.rs` (`pub mod syscall;`)
- Modify: `crates/omni-linux/src/lib.rs` (`install_all` calls `root::syscall::install(table)`)
- Modify: `crates/omni-linux/src/root/profile.rs` (add `Profile::elevation`)
- Test: `crates/omni-linux/src/root/profile.rs` (elevation decision) and `crates/omni-linux/src/sys.rs` (assume)

**Interfaces:**
- Consumes: `Profile::su_allowed`; `Profile::of(instance)` (added in Task 5 — until then, Task 4's handler reads the profile via a helper `root::profile_of(&Process) -> Option<Arc<Profile>>` that Task 5 implements; for Task 4 add a temporary `profile_of` that reads/parses `<instance>/data/adb/omni/profile` each call, which Task 5 replaces with the cached `Profile::of`).
- Produces:
  - `Profile::elevation(&self, caller_uid: u32, target_uid: u32) -> Result<Ids, i32>` where `Ids { uid: u32, gid: u32, groups: Vec<u32>, caps: u64 }`; `Err(EACCES)` when `!su_allowed(caller_uid)`. For `target_uid == 0`: caps = `ALL_CAPS`, groups = `[0]`. For another target: caps = 0.
  - `SysState::assume(&self, uid: u32, gid: u32, groups: Vec<u32>, caps: u64)`
  - ops: `const OP_ELEVATE: u64 = 1; const OP_STATUS: u64 = 2;`
  - handler `sys_omni_root(&Process, &mut Task, [u64;6]) -> SysResult`:
    - no profile or `!profile.is_rooted()` → `Err(ENOSYS)` (looks nonexistent)
    - `OP_ELEVATE`: `target = a[1] as u32` (default 0 if `u32::MAX`); `profile.elevation(p.sys.uid(), target)?` then `p.sys.assume(...)`; return `Ok(0)`
    - `OP_STATUS`: return `Ok(u64::from(profile.magisk_version_code()))`
    - unknown op → `Err(EINVAL)`

- [ ] **Step 1: Write the failing tests**

In `root/profile.rs`:
```rust
#[test]
fn elevation_grants_root_and_refuses_a_disallowed_uid() {
    let p = Profile::parse("root=1\nsu=all\n");
    let ids = p.elevation(10234, 0).expect("allowed");
    assert_eq!(ids.uid, 0);
    assert_eq!(ids.caps, crate::sys::ALL_CAPS);
    let p = Profile::parse("root=1\nsu=com.one\n");
    assert_eq!(p.elevation(10234, 0).unwrap_err(), crate::errno::EACCES.0 as i32);
    // shell may still become root
    assert!(p.elevation(2000, 0).is_ok());
}
```
In `sys.rs`:
```rust
#[test]
fn assume_sets_ids_and_caps() {
    let s = SysState::new(1, 2000);
    s.assume(0, 0, vec![0], ALL_CAPS);
    assert_eq!(s.uid(), 0);
    assert_eq!(s.caps(), ALL_CAPS);
}
```
(Confirm `errno::EACCES`/`ENOSYS`/`EINVAL` representation; use the crate's `Errno` as the other handlers do and compare via its inner value.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p omni-linux --lib 'root::profile::tests::elevation' && cargo test -p omni-linux --lib 'sys::' -- assume`
Expected: FAIL — `elevation` / `assume` not found.

- [ ] **Step 3: Write minimal implementation**

Add `OMNI_ROOT = 510 => "omni_root"` to `syscall.rs`. Add `SysState::assume` (store uid, gid, `*self.groups.lock() = groups`, `set_caps(caps)`). Add `Profile::elevation` (as speced). Write `root/syscall.rs` with `OP_*`, `sys_omni_root`, and `pub fn install(table: &mut Table) { table.set(nr::OMNI_ROOT, sys_omni_root); }`. Add the temporary `root::profile_of(&Process)` reading `<instance>/data/adb/omni/profile` (instance dir via `p.vfs.binds().instance_dir()`). Wire `install` into `lib.rs::install_all`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p omni-linux --lib root:: && cargo test -p omni-linux --lib sys::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/syscall.rs crates/omni-linux/src/sys.rs crates/omni-linux/src/root/ crates/omni-linux/src/lib.rs
git commit -m "feat(root): the omni_root syscall -- ELEVATE raises an allowed caller to uid 0"
```

---

## Task 5: The root layer over `/system`

**Files:**
- Create: `crates/omni-linux/src/root/layer.rs`
- Modify: `crates/omni-linux/src/root/mod.rs` (`pub mod layer;`, and move `profile_of` here as `Profile::of` cached per instance, reloaded on the profile file's mtime like `Binds::of`)
- Modify: `crates/omni-linux/src/vfs.rs` (add `root_layer: Option<Arc<root::Layer>>` to `Vfs`; consult it in `lookup` after binds/writable and before the sysroot manifest; merge its children in `list`; add `with_root_layer`; carry it through `for_exec`)
- Test: `crates/omni-linux/src/root/layer.rs` (`#[cfg(test)]`, layer build) and `crates/omni-linux/tests/vfs_root_layer.rs` (Vfs lookup/list merge)

**Interfaces:**
- Consumes: `Profile`, `Catalog`, `Module` (Tasks 1–2); `root::tools::magisk_binary()` (Task 6 — until Task 6 lands, the tool files resolve to a 1-byte placeholder so Task 5's tests assert *presence and path mapping*, and Task 6 swaps in the real bytes).
- Produces:
  - `struct Layer { files: BTreeMap<Vec<u8>, LayerNode> }` where `LayerNode` is `File { host: PathBuf } | Bytes(Arc<Vec<u8>>) | Dir`
  - `fn Layer::build(profile: &Profile, catalog: &Catalog, instance: &Path) -> Layer`
  - `fn Layer::of(instance: &Path) -> Option<Arc<Layer>>` — cached per instance dir (static map, like `Binds::of`), rebuilt when `<instance>/data/adb/omni/layer.gen` mtime changes or on first call when a profile exists; `None` when no profile or not rooted
  - `fn Layer::lookup(&self, path: &[u8]) -> Option<crate::vfs::Node>`
  - `fn Layer::children(&self, dir: &[u8]) -> Vec<(Vec<u8>, crate::vfs::Node)>`
  - On `Vfs`: `fn with_root_layer(self, layer: Option<Arc<root::Layer>>) -> Self`

Mapping rules in `build`:
- Tool files always present when rooted: `/system/bin/su`, `/system/bin/magisk`, `/system/bin/resetprop` → `Bytes(magisk_binary())` (mode 0755, owner root — recorded via the instance `Owners`); `/debug_ramdisk/magisk`, `/debug_ramdisk/su`, `/debug_ramdisk/resetprop` → same.
- For each enabled module (`profile.modules()` order; skip if the module dir has `disable`/`remove`/`skip_mount`): every file under its `system/` maps onto `/system/<rest>`; `system/vendor/**`→`/vendor/**`, `system/product/**`→`/product/**`, `system/system_ext/**`→`/system_ext/**`. A later module overwrites an earlier one. A directory that contains a `.replace` marker replaces the whole image directory (record the replaced dir so `children` returns only the module's entries under it and `lookup` of an image child under it returns `None`).
- The host source for a module file is the module dir (or, for a zip module, an extracted copy under `<instance>/data/adb/modules/<id>/` — Task 8 does the extraction; Task 5's `build` reads from `<instance>/data/adb/modules/<id>/` directories, which is where installed modules live).

- [ ] **Step 1: Write the failing tests**

`root/layer.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    // Build a layer from two hand-made installed modules under a temp instance and assert mapping.
    #[test]
    fn overlays_adds_replaces_and_vendor_remap() {
        let inst = tmp_instance();
        install_fake_module(&inst, "a", &[
            ("system/etc/added.txt", b"A"),
            ("system/bin/sh", b"replaced-sh"),              // replaces an image file
            ("system/vendor/lib/v.so", b"V"),               // -> /vendor/lib/v.so
        ], &[]);
        install_fake_module(&inst, "b", &[
            ("system/etc/added.txt", b"B"),                 // later module wins
            ("system/fonts/.replace", b""),
            ("system/fonts/only.ttf", b"F"),
        ], &[]);
        let profile = crate::root::Profile::parse("root=1\nmodule=a\nmodule=b\n");
        let cat = crate::root::module::Catalog::discover(&inst.join("data/adb/modules"), None).unwrap();
        let layer = Layer::build(&profile, &cat, &inst);

        assert!(matches!(layer.lookup(b"/system/bin/su"), Some(_)));                 // tool present
        assert_eq!(read_bytes(&layer, b"/system/etc/added.txt"), Some(b"B".to_vec())); // later wins
        assert!(layer.lookup(b"/system/bin/sh").is_some());                          // replaces image file
        assert!(layer.lookup(b"/vendor/lib/v.so").is_some());                        // vendor remap
        // .replace: image children under /system/fonts are hidden; only.ttf remains
        let fonts: Vec<_> = layer.children(b"/system/fonts").into_iter().map(|(n,_)| n).collect();
        assert_eq!(fonts, vec![b"only.ttf".to_vec()]);
    }

    #[test]
    fn disabled_and_skip_mount_modules_are_ignored() {
        let inst = tmp_instance();
        install_fake_module(&inst, "a", &[("system/etc/x.txt", b"A")], &["disable"]);
        let profile = crate::root::Profile::parse("root=1\nmodule=a\n");
        let cat = crate::root::module::Catalog::discover(&inst.join("data/adb/modules"), None).unwrap();
        let layer = Layer::build(&profile, &cat, &inst);
        assert!(layer.lookup(b"/system/etc/x.txt").is_none());
    }
}
```
`tests/vfs_root_layer.rs`:
```rust
// Build a Vfs from a tiny manifest plus a root layer; a layer file shadows / adds to the image,
// and list() merges the layer's children with the image's.
#[test]
fn vfs_sees_layer_over_the_image() {
    // ... construct Sysroot::from_manifest with /system/etc/keep.txt ...
    // attach a Layer adding /system/etc/added.txt and replacing /system/etc/keep.txt
    // assert resolve(/system/etc/added.txt) is the layer's, resolve(/system/etc/keep.txt) is the layer's,
    // and list(/system/etc) contains both names once.
}
```
Provide the `tmp_instance`, `install_fake_module`, `read_bytes` helpers in the test module (write `module.prop` with the id, the listed files, and the marker files).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p omni-linux --lib root::layer && cargo test -p omni-linux --test vfs_root_layer`
Expected: FAIL — `Layer` / `with_root_layer` not found.

- [ ] **Step 3: Write minimal implementation**

Build the `BTreeMap` per the mapping rules (tools first, then modules in order, later wins; track replaced dirs in a `BTreeSet<Vec<u8>>`). `lookup` returns a `Node::HostFile`/`Node::Blob`-like node — reuse `vfs::Node`: a `Bytes` layer node needs a node variant that carries bytes. The image uses `Node::SysFile { size, mode }` backed by the sysroot's object store; the layer's bytes aren't there. Two options — pick the simpler: **(a)** give the layer host-backed files only (write tool bytes to `<instance>/data/adb/omni/tools/su` at build and map as `Node::HostFile { host }`), so every layer node is `HostFile`/`HostDir` and no new `Node` variant is needed. Do (a). Then `Vfs::lookup` consults `self.root_layer` after the bind/writable loop: `if let Some(l) = &self.root_layer { if let Some(n) = l.lookup(path) { return Some(n); } if l.hides(path) { return None; } }` — `hides` returns true for an image child under a `.replace` dir. `Vfs::list` for a `Node::Dir`: after collecting synthetic + sysroot children, add the layer's `children(dir)` (dedup by name, layer wins) and drop any image child the layer hides. Carry `root_layer` through `for_exec`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p omni-linux --lib root:: && cargo test -p omni-linux --test vfs_root_layer && cargo test -p omni-linux --test vfs`
Expected: PASS (new tests, and the existing `tests/vfs.rs` still green).

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/root/layer.rs crates/omni-linux/src/root/mod.rs crates/omni-linux/src/vfs.rs crates/omni-linux/tests/vfs_root_layer.rs
git commit -m "feat(root): the root layer over /system (module overlays, .replace, vendor remap)"
```

---

## Task 6: The `magisk` multi-call binary

**Files:**
- Create: `crates/omni-linux/device/src/root/magisk.c`
- Create: `crates/omni-linux/device/src/root/build.txt` (the exact NDK command, mirroring `device/src/build.txt`)
- Create (committed build output): `crates/omni-linux/device/root/magisk` (the arm64 ELF)
- Modify: `crates/omni-linux/device/SHA256SUMS` (add the binary's sha)
- Create: `crates/omni-linux/src/root/tools.rs` (`magisk_binary() -> &'static [u8]` via `include_bytes!`), re-export from `root/mod.rs`
- Modify: `crates/omni-linux/src/root/layer.rs` (tool nodes use `tools::magisk_binary()` bytes)
- Test: `crates/omni-linux/src/root/tools.rs` (sha of the embedded bytes matches `SHA256SUMS`) and reuse Task 5's layer test (now the tool bytes are real)

**Interfaces:**
- Produces: `omni_linux::root::tools::magisk_binary() -> &'static [u8]`

The C program (freestanding of bionic niceties; link libc as `mapper.c` does). Applets by `basename(argv[0])` or `argv[1]`:
- `su`: parse `su [options] [uid] [-c CMD...]` minimally — flags `-`, `-l`, `-p`, `-mm`/`--mount-master` accepted and ignored; a bare numeric arg = target uid (default 0); `-c` = rest is one `sh -c` command. Issue `omni_root(OP_ELEVATE, uid)` via inline `svc`; on `-EACCES` print `Permission denied` to stderr and `exit(1)`; else `execv("/system/bin/sh", {sh, ["-c", cmd] | ["-"], NULL})`.
- `resetprop`: `resetprop [-n] [--delete] NAME [VALUE]` / `--file F` — but props bypass is a syscall-less path: resetprop talks to the property service socket for `set`? No — the ro. bypass is `set_forced`, reachable only engine-side. So `resetprop` issues a dedicated op: add `OP_SETPROP`/`OP_DELPROP` to `omni_root` (extend Task 4's handler in this task, calling `PropertyService::global(..).set_forced/delete`). resetprop passes name/value pointers in registers (the engine reads guest memory as other syscalls do via `p.mem.read_cstr`).
- `magisk`: `-v`→print `<ver>:MAGISK:R` (ver from `OP_STATUS`), `-V`→print the version code, `--path`→print `/debug_ramdisk`, `-c`→same as `-v`; `--denylist`/`--sqlite`→print `unsupported in omnidroid (R1)` and `exit(1)`.

Because `OP_SETPROP`/`OP_DELPROP` extend the handler, this task also modifies `crates/omni-linux/src/root/syscall.rs` and must read guest strings. Add the two ops and a layer-test-independent Rust unit test for the string-reading path only if feasible; otherwise cover via Task 8's boot test (`resetprop ro.build.tags test-keys` then `getprop`).

- [ ] **Step 1: Write the failing test**

```rust
// crates/omni-linux/src/root/tools.rs
#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    #[test]
    fn embedded_magisk_matches_sha256sums() {
        let bytes = super::magisk_binary();
        assert!(bytes.len() > 64, "a real arm64 ELF, not the placeholder");
        let got = format!("{:x}", Sha256::digest(bytes));
        let sums = include_str!("../../device/SHA256SUMS");
        assert!(sums.lines().any(|l| l.starts_with(&got) && l.contains("root/magisk")),
            "magisk sha {got} is not pinned in device/SHA256SUMS");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p omni-linux --lib root::tools`
Expected: FAIL — `tools`/`magisk_binary` not found (and no committed binary yet).

- [ ] **Step 3: Build the binary and write the implementation**

Write `magisk.c`. Build:
```bash
NDK=~/android-ndk/android-ndk-r28c
"$NDK/toolchains/llvm/prebuilt/windows-x86_64/bin/aarch64-linux-android35-clang.cmd" \
  -O2 -Wall -Wextra -Werror -static-libgcc -o crates/omni-linux/device/root/magisk \
  crates/omni-linux/device/src/root/magisk.c
python - <<'PY'
import hashlib,pathlib
b=pathlib.Path("crates/omni-linux/device/root/magisk").read_bytes()
print(hashlib.sha256(b).hexdigest(), "*root/magisk")
PY
```
Append the printed line to `device/SHA256SUMS`. Write `tools.rs` with `pub fn magisk_binary() -> &'static [u8] { include_bytes!("../../device/root/magisk") }`. Point Task 5's tool nodes at these bytes (write them to `<instance>/data/adb/omni/tools/magisk` in `Layer::build` and map `/system/bin/su` → that host file). Extend `root/syscall.rs` with `OP_SETPROP`/`OP_DELPROP` reading guest cstrings and calling `set_forced`/`delete`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p omni-linux --lib root::`
Expected: PASS (tools sha + the layer tests now carry real bytes).

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/device/src/root crates/omni-linux/device/root/magisk crates/omni-linux/device/SHA256SUMS crates/omni-linux/src/root/tools.rs crates/omni-linux/src/root/layer.rs crates/omni-linux/src/root/syscall.rs crates/omni-linux/src/root/mod.rs
git commit -m "feat(root): the magisk multi-call binary (su/resetprop/magisk) and its SETPROP ops"
```

---

## Task 7: Magisk installer assets (fetch tool + pin + accessor)

**Files:**
- Create: `tools/fetch_magisk.py`
- Create: `tools/magisk.pin` (version, versionCode, url, sha256 of the APK — the latest stable release at build time)
- Modify: `.gitignore` (add `/sysroot/magisk-*/`)
- Create: `crates/omni-linux/src/root/assets.rs` (`MagiskAssets { util_functions: PathBuf, busybox: PathBuf }`, `fn find(repo_root: &Path) -> Result<MagiskAssets, String>` reading `sysroot/magisk-<ver>/`), re-export from `root/mod.rs`
- Test: `crates/omni-linux/src/root/assets.rs` (pin parse; a guarded `find` that returns a clear error when absent)

**Interfaces:**
- Produces:
  - `struct MagiskPin { pub version: String, pub version_code: u32, pub url: String, pub sha256: String }`, `fn MagiskPin::parse(text: &str) -> Result<MagiskPin, String>`
  - `struct MagiskAssets { pub util_functions: PathBuf, pub busybox: PathBuf, pub version_code: u32 }`
  - `fn MagiskAssets::find(repo_root: &Path) -> Result<MagiskAssets, String>` — `Err` says "run `python tools/fetch_magisk.py`" when `sysroot/magisk-<ver>/` is missing.

`fetch_magisk.py`: read `tools/magisk.pin`; download `url` to a temp file; verify sha256 == pin; open the APK (a zip) and extract `assets/util_functions.sh` and `lib/arm64-v8a/libbusybox.so` → `sysroot/magisk-<version>/util_functions.sh` and `.../busybox`; print where they landed. Refuse (non-zero exit) on sha mismatch.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_the_pin() {
        let p = MagiskPin::parse("version=v29.0\nversionCode=29000\nurl=https://example/m.apk\nsha256=abcd\n").unwrap();
        assert_eq!(p.version_code, 29000);
        assert_eq!(p.sha256, "abcd");
    }
    #[test]
    fn find_explains_when_absent() {
        let err = MagiskAssets::find(std::path::Path::new("/no/such/repo")).unwrap_err();
        assert!(err.contains("fetch_magisk.py"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p omni-linux --lib root::assets`
Expected: FAIL — `assets` not found.

- [ ] **Step 3: Write the implementation + fetch the assets**

Write `assets.rs`, `tools/fetch_magisk.py`, `tools/magisk.pin` (fill in the real latest-stable values; compute the APK sha once and record it). Run `python tools/fetch_magisk.py` to populate `sysroot/magisk-<ver>/`. Add the gitignore line.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p omni-linux --lib root::assets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tools/fetch_magisk.py tools/magisk.pin .gitignore crates/omni-linux/src/root/assets.rs crates/omni-linux/src/root/mod.rs
git commit -m "feat(root): Magisk installer assets -- pinned fetch tool and the util_functions/busybox accessor"
```

---

## Task 8: Boot wiring and the rooted boot test

**Files:**
- Create: `crates/omni-linux/device/vendor/etc/init/omni_root.rc`
- Create: `crates/omni-linux/device/vendor/bin/omni_root.sh`
- Modify: `crates/omni-linux/src/device.rs` (add both to `FILES`)
- Modify: `crates/omni-linux/src/process.rs` (`spawn_as` and `exec_image`: attach `root::Layer::of(&config.instance_dir)` via `.with_root_layer(...)`)
- Create: `crates/omni-linux/src/root/install.rs` (the host-side staging the launcher calls: write the profile, copy selected module zips to `<instance>/data/adb/omni/pending/<id>.zip`, copy `util_functions.sh`+`busybox` to `<instance>/data/adb/magisk/`, `disable` deselected modules). `fn stage(instance: &Path, profile: &Profile, catalog: &Catalog, assets: &MagiskAssets) -> Result<(), String>`
- Create: `crates/omni-linux/tests/data/omni-test-module/` (module.prop, system/etc/omni-test.txt, system/<replace-dir>/.replace + a file, system.prop, post-fs-data.sh, service.sh, customize.sh)
- Create: `crates/omni-linux/tests/root_boot.rs` (`#[ignore]`, uses `common::boot::Boot`)
- Test: `crates/omni-linux/tests/root_boot.rs`

**Interfaces:**
- Consumes: Tasks 1–7.
- Produces: `root::install::stage`.

`omni_root.rc` (device overlay — present on every device, but the script exits immediately when `/data/adb/omni/profile` is absent, so a non-rooted device is unchanged):
```
on post-fs-data
    exec_start omni_root_pfd

service omni_root_pfd /system/bin/sh /vendor/bin/omni_root.sh post-fs-data
    user root
    group root
    oneshot
    disabled

service omni_root_service /system/bin/sh /vendor/bin/omni_root.sh service
    class late_start
    user root
    group root
    oneshot
```
(`exec_start omni_root_pfd` runs in the `on post-fs-data` boot phase — `init.rs` runs boot-phase commands before `class_start`, and system_server is the main program, so modules install before the framework starts. Confirm with the boot test's ordering assertion.)

`omni_root.sh`: `[ -f /data/adb/omni/profile ] || exit 0`; for `post-fs-data`: install each `/data/adb/omni/pending/*.zip` via `busybox sh` with `util_functions.sh`'s `install_module` (env `BOOTMODE=true MODPATH=/data/adb/modules/<id> ZIPFILE=<zip> OUTFD=1`), log to `/data/adb/omni/install.log`; apply each enabled module's `system.prop` with `resetprop --file`; run each `post-fs-data.sh`; `touch /data/adb/omni/layer.gen`. For `service`: run each enabled module's `service.sh` in the background. Prefix log lines with `[omni-root]`.

- [ ] **Step 1: Write the failing test**

`tests/root_boot.rs`:
```rust
mod common;
use std::time::Duration;

#[test]
#[ignore = "boots the whole system to a root shell: minutes"]
fn a_rooted_device_grants_root_and_applies_a_module() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = /* a fresh temp dir */;
    // Stage: profile root=1 module=omni-test, copy the test module as an installed module, write assets.
    stage_test_root(&instance);
    // A --then shell that exercises everything and prints markers:
    let then = "\
        echo RB_UID=$(su -c id -u); \
        echo RB_UID2000=$(su 2000 -c id -u); \
        echo RB_FILE=$(cat /system/etc/omni-test.txt); \
        echo RB_PROP=$(getprop ro.omni.test); \
        su -c 'resetprop ro.build.tags omni-test'; echo RB_RESET=$(getprop ro.build.tags); \
        echo RB_PFD=$(cat /data/local/tmp/omni-test-pfd 2>/dev/null); \
        echo RB_SVC=$(cat /data/local/tmp/omni-test-svc 2>/dev/null); \
        echo RB_VER=$(magisk -v); \
        echo RB_DONE";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let mut seen = std::collections::HashMap::new();
    boot.watch(Duration::from_secs(600), |line| {
        for key in ["RB_UID=","RB_UID2000=","RB_FILE=","RB_PROP=","RB_RESET=","RB_PFD=","RB_SVC=","RB_VER="] {
            if let Some(v) = line.strip_prefix(key) { seen.insert(key, v.trim().to_string()); }
        }
        line.contains("RB_DONE")
    });
    assert_eq!(seen.get("RB_UID="), Some(&"0".to_string()));
    assert_eq!(seen.get("RB_UID2000="), Some(&"2000".to_string()));
    assert_eq!(seen.get("RB_FILE="), Some(&"omni-test".to_string()));  // system/etc/omni-test.txt holds "omni-test" (no trailing newline)
    assert_eq!(seen.get("RB_PROP="), Some(&"1".to_string()));
    assert_eq!(seen.get("RB_RESET="), Some(&"omni-test".to_string()));
    assert_eq!(seen.get("RB_PFD="), Some(&"ok".to_string()));
    assert_eq!(seen.get("RB_SVC="), Some(&"ok".to_string()));
    assert!(seen.get("RB_VER=").is_some_and(|v| v.contains(":MAGISK:R")));
}

#[test]
#[ignore = "boots the whole system without a profile: minutes"]
fn a_device_without_a_profile_is_not_rooted() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = /* fresh temp dir, NO stage_test_root */;
    let then = "echo RB_SU=$(command -v su || echo none); \
                echo RB_DBG=$(ls /debug_ramdisk 2>/dev/null || echo none); \
                echo RB_FILE=$(cat /system/etc/omni-test.txt 2>/dev/null || echo none); echo RB_DONE";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let mut seen = std::collections::HashMap::new();
    boot.watch(Duration::from_secs(600), |line| {
        for key in ["RB_SU=","RB_DBG=","RB_FILE="] { if let Some(v)=line.strip_prefix(key){ seen.insert(key, v.trim().to_string()); } }
        line.contains("RB_DONE")
    });
    assert_eq!(seen.get("RB_SU="), Some(&"none".to_string()));
    assert_eq!(seen.get("RB_DBG="), Some(&"none".to_string()));
    assert_eq!(seen.get("RB_FILE="), Some(&"none".to_string()));
}
```
Write `stage_test_root` (calls `root::install::stage` with a profile `root=1\nmodule=omni-test\n`, a catalog pointing at `tests/data`, and either the real assets via `MagiskAssets::find(repo_root)` or — if absent — a minimal shim `util_functions.sh` so the test does not require the GPL download; prefer the real assets, skip the module-install assertions if absent but still assert su/layer/props). The test module's `post-fs-data.sh` writes `ok` to `/data/local/tmp/omni-test-pfd`, `service.sh` writes `/data/local/tmp/omni-test-svc`, `system.prop` sets `ro.omni.test=1`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p omni-linux --test root_boot -- --ignored --nocapture`
Expected: FAIL — no root layer attached at spawn / `omni_root.sh` not in the image / `stage` missing.

- [ ] **Step 3: Write the implementation**

Add `omni_root.rc`/`omni_root.sh` to `device.rs::FILES`. In `process.rs`, attach the layer: `.with_root_layer(crate::root::Layer::of(&config.instance_dir))` in both `spawn_as` (on the `Vfs::new(...)` chain) and `exec_image` (on `for_exec`). Write `root::install::stage`. Make the test module fixtures.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p omni-linux --test root_boot -- --ignored --nocapture`
Expected: PASS (both tests). Also run the fast suite to confirm nothing regressed: `cargo test -p omni-linux --lib && cargo test -p omni-linux --test vfs --test props --test mount --test exec`.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/device/vendor/etc/init/omni_root.rc crates/omni-linux/device/vendor/bin/omni_root.sh crates/omni-linux/src/device.rs crates/omni-linux/src/process.rs crates/omni-linux/src/root/install.rs crates/omni-linux/src/root/mod.rs crates/omni-linux/tests/data/omni-test-module crates/omni-linux/tests/root_boot.rs
git commit -m "feat(root): boot wiring -- stage modules, install at post-fs-data, attach the layer at spawn"
```

---

## Task 9: CLI, launcher staging, golden key, warm, MCP

**Files:**
- Modify: `crates/omnidroid/src/main.rs` (`AospOptions` + `parse_aosp` + `aosp_env`; a new `modules` subcommand; pass staging inputs through)
- Modify: `crates/omni-linux/tests/r_roblox.rs` (read `OMNI_R_ROOT`/`OMNI_R_MODULES`/`OMNI_R_SU`, call `root::install::stage` before boot, extend `golden_dir` with a root hash)
- Modify: `crates/omni-warm/src/lib.rs` (record the profile hash in the warm device dir; a differing request does not reuse)
- Modify: `crates/omni-mcp/src/server.rs` (`start_instance` gains `root`/`modules`; `launcher` passes `--root`/`--module`; `device_status`/`list_instances` report the profile)
- Test: `crates/omnidroid/src/main.rs` (parse_aosp) and `crates/omni-linux/tests/r_roblox.rs` (golden_dir root hash) — both pure unit tests

**Interfaces:**
- Consumes: `root::install::stage`, `Profile`, `Catalog`, `MagiskAssets`.
- Produces: new CLI flags `--root`, `--module a,b,c` (implies `--root`), `--su all|pkg,pkg`; env vars `OMNI_R_ROOT=1`, `OMNI_R_MODULES=a,b`, `OMNI_R_SU=all`; `omnidroid modules [list|add <zip>]`.

- [ ] **Step 1: Write the failing tests**

In `main.rs` tests:
```rust
#[test]
fn parse_aosp_reads_root_and_modules() {
    let o = parse_aosp(["--module","zygisk-frida,emu-hide","--su","all"].iter().map(|s| s.to_string())).unwrap();
    assert!(o.root);
    assert_eq!(o.modules, vec!["zygisk-frida".to_string(), "emu-hide".to_string()]);
    assert_eq!(o.su.as_deref(), Some("all"));
    // --root alone
    let o = parse_aosp(["--root"].iter().map(|s| s.to_string())).unwrap();
    assert!(o.root && o.modules.is_empty());
}

#[test]
fn unknown_module_id_is_rejected() {
    // choose/validate against a catalog built from an empty dir -> error naming the id
    let err = validate_modules(&["ghost".to_string()], &empty_catalog()).unwrap_err();
    assert!(err.contains("ghost"));
}
```
In `r_roblox.rs` tests (add a `#[cfg(test)] mod tests` or a plain `#[test]`, matching the file's style):
```rust
#[test]
fn golden_key_separates_rooted_devices_and_leaves_unrooted_unchanged() {
    let root = std::env::temp_dir();
    let apk = root.join("x.apk");
    let plain = golden_dir(&root, &apk, None, true, "tr-TR");
    let rooted = golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&root_hash("root=1\nmodule=a\n", &[("a","sha_a")])));
    let rooted_b = golden_dir_rooted(&root, &apk, None, true, "tr-TR", Some(&root_hash("root=1\nmodule=a\nmodule=b\n", &[("a","sha_a"),("b","sha_b")])));
    assert_ne!(plain, rooted);               // rooted differs from unrooted
    assert_ne!(rooted, rooted_b);            // module set changes the key
    assert_eq!(plain, golden_dir(&root, &apk, None, true, "tr-TR")); // unrooted stable
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p omnidroid parse_aosp_reads_root && cargo test -p omni-linux --test r_roblox golden_key_separates`
Expected: FAIL — fields/functions not found.

- [ ] **Step 3: Write the implementation**

Add `root: bool`, `modules: Vec<String>`, `su: Option<String>` to `AospOptions`; parse `--root`/`--module`/`--su` (`--module` sets `root = true`); extend `aosp_env`. Add `validate_modules(ids, catalog) -> Result<(), String>` and call it in `aosp()` before launch (fail fast on an unknown id — Review Focus). Add the `modules` subcommand (`list` prints the catalog; `add <zip>` copies into the user dir after a `ModuleProp::parse` check). In `r_roblox.rs`: read the env, build the catalog (`root::module::Catalog::discover(builtin, user)`) and profile, call `root::install::stage` before `Boot`/the runner; add `golden_dir_rooted` (or extend `golden_dir` with an `Option<&str> root_hash` appended as `-root-<hash8>` when `Some`) and `root_hash(profile_text, &[(id, sha)])` (sha256 of the profile text + each module's sha + the Magisk versionCode + the `magisk` binary sha). In `omni-warm`: write `<device>/root-hash` at creation, and in `usable()`/reuse, skip a device whose hash differs from the request (log `[warm] root profile differs: booting a new device`). In `omni-mcp`: `start_instance` reads `root`(bool)/`modules`(array); `launcher` appends `--root`/`--module`; `device_status` includes the profile summary.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p omnidroid && cargo test -p omni-linux --test r_roblox golden_key_separates && cargo build -p omni-mcp -p omni-warm`
Expected: PASS; both binaries build.

- [ ] **Step 5: Commit**

```bash
git add crates/omnidroid/src/main.rs crates/omni-linux/tests/r_roblox.rs crates/omni-warm/src/lib.rs crates/omni-mcp/src/server.rs
git commit -m "feat(root): --root/--module/--su, modules subcommand, rooted golden key, warm hash, MCP params"
```

---

## Task 10: Community-module smoke, su-probe app, cross-host

**Files:**
- Modify: `crates/omni-linux/tests/root_boot.rs` (add a community-module smoke test behind an env guard)
- Create: `crates/omni-linux/tests/data/su-probe/` (a minimal APK source or a prebuilt test APK whose activity runs `su -c id` and logs it) — or reuse `tools/make_test_apks.sh` if a suitable stub exists
- Create: `docs/NIGHT-2026-10-03-root.md` (the cross-host run record, per the owner's cross-host rule)

**Interfaces:**
- Consumes: the whole R1 stack.

- [ ] **Step 1: Write the smoke test (guarded, so CI without the module is green)**

```rust
#[test]
#[ignore = "installs a real community module; needs OMNI_ROOT_SMOKE_MODULE set to a module zip"]
fn mhpc_installs_and_runs() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(zip) = std::env::var_os("OMNI_ROOT_SMOKE_MODULE") else { return };
    // stage with that module, boot, assert install.log has no "! " error lines and `su -c props` runs.
}
```

- [ ] **Step 2: Run it to verify it fails / skips**

Run: `cargo test -p omni-linux --test root_boot mhpc_installs -- --ignored`
Expected: skips (no env) or FAILs if the stack is wrong.

- [ ] **Step 3: Implement + run the real thing**

Download **MagiskHide Props Config** to the user module dir, run with `OMNI_ROOT_SMOKE_MODULE=<zip>`, confirm `install.log` has no `! `/`abort` lines and `su -c props` prints its menu. Build/obtain the `su-probe` APK, install it on a warm rooted device (`omnidroid aosp --root --su all` then `install_apk`), confirm the activity logs `uid=0`, and that with `--su com.other.pkg` the probe is refused.

- [ ] **Step 4: Cross-host**

Per [[cross-host-game-check]]: once Windows is green, sync Mac (`berat@macmini.local`) and Linux (`berat@192.168.0.38`) to this branch, run `cargo test -p omni-linux --test root_boot -- --ignored` on each, and record results (and any `AndroidRootedKick` behavior seen) in `docs/NIGHT-2026-10-03-root.md`. Screenshot a rooted `su -c id` to the owner.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/tests/root_boot.rs crates/omni-linux/tests/data/su-probe docs/NIGHT-2026-10-03-root.md
git commit -m "test(root): community-module smoke, su-probe app, and the cross-host run record"
```

---

## Notes for the executor

- **Run order matters for the fast suite.** After each task, `cargo test -p omni-linux --lib` must stay green; the `#[ignore]` boot tests run only in Tasks 8 and 10.
- **The `Profile::of` cache** (Task 5) and `Binds::of` share the per-instance, mtime-reload idiom — copy it, do not invent a new one.
- **Keep `lookup` cheap.** The root layer is consulted on every path resolution; `Layer::lookup` is a `BTreeMap` get, and `Vfs` holds `Option<Arc<Layer>>` so a non-rooted device pays one `Option` check.
- **Do not thread a `hidden` flag through `lookup` in R1.** R2 adds per-process hiding by giving the app's `Vfs` a layer that already excludes hidden content, or a `hidden` field on `Vfs`. R1's seam is `Profile::hidden()` returning `false`.
