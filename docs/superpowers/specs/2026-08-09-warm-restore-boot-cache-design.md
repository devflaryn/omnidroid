# Warm-restore boot cache — design

**Date:** 2026-08-09
**Goal:** `omnidroid start <account>` lands the account inside its Roblox place in
under ~15 s, on any host OS and either architecture, without weakening
stability, auto-login, auto-capture, or the omni-executor control surface.

---

## 1. Problem

`omnidroid start` today runs a full cold boot on every launch:

```
reconcile_runtime -> ensure_qemu -> Roblox cookie preflight (network)
  -> spawn_qemu (arm: EDK2/UEFI -> GRUB -> kernel -> full Android cold boot)
  -> wait_for_boot (5 s poll on sys.boot_completed)     ~31-40 s (CHANGELOG)
  -> post_boot / _enforce_hiding / assert_kiosk_game / mode tuning
  -> deliver_session (pm grants -> force-stop Roblox -> kiosk broadcast
                      -> Roblox cold start -> join place)
```

Android is re-booted from scratch for a machine that is byte-for-byte the same
every time. Nothing in the engine saves machine state: the existing
`snapshot=on` is qcow2 disk copy-on-write (writes discarded at exit), not saved
RAM/device state.

The Android half is therefore ~35 s of pure repeated work. The Roblox half
(cold start + join) is **unmeasured** — Phase 0 exists to measure it.

## 2. Verified facts (probed on the primary host, 2026-08-09)

Host: macOS / Apple Silicon, QEMU 11.0.2 (Homebrew), `-accel hvf -cpu host`.

| Assumption | Result |
| --- | --- |
| HVF supports machine state save/restore | **Yes.** `savevm`/`loadvm` both returned clean with vCPUs having run (`VM_CLOCK 00:04.985`). No migration blocker is registered for HVF. |
| `migrate file:` with `mapped-ram` + `multifd` | **Yes.** Both capabilities accepted; migration reached `completed`. |
| VM can resume after a file migration | **Yes.** `cont` moves `postmigrate` -> `running`. A bake does not waste the boot it was taken from. |
| State file cost | **Sparse.** 647 MB apparent / **5.1 MB actual** for a 512 MB guest. The probe guest had no OS, so nearly every page was zero; a booted Android guest will be far larger. The load-bearing fact is that `mapped-ram` stores only touched pages, so an 8 GB `playable` entry is expected to cost roughly its resident set (order 1.5-2.5 GB), not 8 GB. Phase 1 records the real figure. |
| `direct-io` migration parameter | **Not available** in this build (`No build-time support for direct-io`). Optional; enabled only where the build accepts it. |
| WHPX (Windows) migration support | **UNVERIFIED.** Spiked in Phase 1. See §8. |

## 2b. Spike results — real arm64 guest (2026-08-09)

§2 was probed on a toy VM. A second spike ran the **real** LineageOS arm64 guest
with the full production headless topology (EDK2 pflash vars, virtio-blk
system+data, virtio-gpu-pci, nec-usb-xhci + tablet + kbd, slirp with hostfwd,
VNC, virtio-balloon with free-page-reporting, virtio-rng), at 4096 MB / 4 vCPU.

| Measurement | Result |
| --- | --- |
| Cold boot to `sys.boot_completed` | **19.5-20.3 s** |
| Bake (`stop` + `migrate file:`) | **17.1-19.5 s**, one-time |
| **Restore to live Android** | **2.8-6.2 s** (migration load alone 2.0-3.1 s) |
| State file | 4.2 GiB apparent / **2.3-2.5 GiB actual** (sparse) |
| Golden entry unmodified by restores | **Yes** — `snapshot=on` sharing holds |
| Post-restore product checks | `boot_completed`, arm64 abilist, adb shell, package manager, kiosk present, VNC serving — **all pass** |

Cold boot measures spawn -> `boot_completed` only, which is why it is below the
31-40 s figure in CHANGELOG (that includes first-boot and post-boot work).

Three findings changed the design:

**(a) Restore needs `-incoming defer`, not `-incoming file:`.** `mapped-ram` and
`multifd` must be enabled on the *destination* before the stream is read, and
capabilities can only be set over QMP. A plain `-incoming file:` dies with
`Capability mapped-ram is off, but received capability is on`. The restore path
is therefore: spawn with `-incoming defer` -> QMP handshake
(`migrate-set-capabilities` + `migrate-set-parameters`) -> `migrate-incoming` ->
wait `completed` -> `cont`.

**(b) Clock skew is real and `-rtc base=utc,clock=host` does NOT fix it.**
Measured skew equalled the wall time between bake and restore exactly (22-28 s
in a test run seconds after baking; a day-old entry wakes a day behind).
`adb shell su -c "date -s @<epoch>"` corrected it to **0 s**. §7's clock step is
mandatory, not defensive.

**(c) BLOCKER — a second concurrent restore from one entry is unreachable over
adb.** See §8b.

## 8b. Open blocker: concurrent restore and adb

**Evidence.** One restored instance works reliably (adb up in 3.1 s). A second
instance restored concurrently from the same golden entry loads its migration
successfully and **is alive** — VNC serves on both — but its adb transport sits
`offline` and never completes the handshake.

Ruled out by experiment:

- *Not* a per-instance defect: the second instance restored **alone** works
  (adb up in 3.1 s).
- *Not* pre-existing: **two concurrent cold boots** from the same shared
  template both stay `device` and both answer adb. Restore introduces this.
- *Not* host-side adb-server deduplication: giving each instance its **own** adb
  server (`ANDROID_ADB_SERVER_PORT`) did not help — the second is still
  `offline`.
- *Not* fixed by quiescing adbd at bake time: `stop adbd; start adbd` before
  freezing made it **worse** (adbd not listening in the frozen image, so *both*
  instances failed). Do not retry this without verifying adbd is actually
  listening at the freeze point.

**Leading hypothesis** (unconfirmed): the guest's adbd carries established
socket/identity state into the snapshot, and every restored instance replays it
identically, so two of them cannot both complete the ADB handshake against the
same host. Root-causing is Phase 1 work, not design work.

**Interim rule, which ships regardless of root cause:**

> Restore only when **no other running instance is using the same golden
> entry**. Otherwise cold-boot.

This is correct under all the evidence above: the first instance restores fast,
any concurrent sibling cold-boots at today's speed, and both work. It preserves
§4's spine, gives the win to the common single-instance `playable` case
immediately, and leaves farming at today's behaviour until the blocker is
solved. Lifting the rule is a Phase 2 item gated on a root cause.

**Not yet verified:** a restored instance running concurrently with a
*cold-booted* one. Phase 1 must test this before the interim rule is trusted for
mixed fleets.

## 3. Mechanism decision

Three options were considered.

**A. `savevm`/`loadvm` (qcow2 internal snapshot).** Rejected. The state lives
inside a writable qcow2, internal snapshots are not visible through a backing
chain, and `loadvm` reverts the disk. N concurrent instances cannot share one,
which fights the shared-template `snapshot=on` model the engine is built on.

**B. File migration (`migrate file:` out, `-incoming file:` in). CHOSEN.**
RAM + device state goes to a standalone file; disks stay independent. Instances
open warm overlays `snapshot=on` exactly as today (concurrency unchanged) and
restore the same state file read-only. One code path on macOS, Windows and
Linux, x86 and arm.

**C. B, plus reflink-cloning the state file per instance and mmapping it
(`x-ignore-shared`).** Rejected for v1. Reflink is APFS/btrfs/XFS-only; NTFS has
no equivalent, so C would require a divergent Windows path — contradicting both
the cross-platform requirement and *stability > speed*. Retained as a documented
future optimization, gated on Phase 0 numbers (§10).

## 4. Design spine

> **The cache is an optimization layer that always degrades to today's cold
> boot.** Any miss, key mismatch, missing/corrupt file, failed bake, or
> unsupported accelerator results in exactly the current behaviour. Nothing in
> this feature is allowed to fail a launch.

This single rule is what makes *stability > speed* structural rather than
aspirational, and it is also the fallback that covers the unverified WHPX case.

## 5. Cache model

### 5.1 Key

```
(arch, base tag, base version, offset name, mode name,
 resolved mem_mb, resolved smp, machine+accel, qemu version)
```

Hashed to a stable directory name.

Invalidation is a **consequence of the key**, not separate bookkeeping:

- A base update bumps `base version` -> every offset's entry for that base
  misses -> each is re-baked lazily on the first launch that needs it.
- A new APK is a new offset name -> new key -> baked on first use.
- A QEMU upgrade changes the migration stream format -> new key.
- `--debug` attaches the devkit disk as vdc, changing device topology.
  **Debug boots bypass the cache entirely** — no lookup, no bake.

Nothing is rebuilt eagerly or in the background, and the user is never asked
about a cache file. Select a base + APK version: if an entry exists, restore;
if not, cold-boot and capture one.

### 5.2 Entry layout

`<images_dir>/warm/<key>/`

| File | Contents |
| --- | --- |
| `state` | Migration stream: RAM + device state (sparse). |
| `system.qcow2` | Warm COW overlay of the base system at the freeze point. |
| `data.qcow2` | Warm COW overlay of the offset's `/data` at the freeze point. |
| `efivars.fd` | The exact UEFI vars file at the freeze point. |
| `meta.json` | Full key, QEMU version, baked mem/smp, base version, timestamp. |

`data.qcow2` is one COW level over the offset's image. This does not violate the
offsets anti-chaining invariant — that rule forbids an *offset* backed by
another offset, so that re-baking stays cheap and deleting one offset cannot
harm another. A warm overlay is a derived cache artifact keyed on the offset;
deleting the offset orphans it and `prune()` reclaims it.

### 5.3 mem/smp pinning

Migration requires identical `-m` and `-smp` between save and restore, but
`playable` autoscales to host capacity at launch time. Resolution:

- The **first cold boot bakes at the host-resolved size**, so on a given machine
  the snapshot is baked at that machine's correct `playable` size — the mode
  still takes the machine, as designed.
- Restores then **pin** mem/smp to `meta.json`, so a shift in host free RAM does
  not silently miss the cache.
- **Host-fit guard:** if the host can no longer afford the baked mem, the
  restore is skipped and the instance cold-boots. Restoring into a host that
  will swap is slower than the smaller cold guest and, on macOS, eventually
  fatal to the process.

### 5.4 Disk budget and eviction

Warm entries are large and the host they must live on is not empty. Measured on
the primary host, 2026-08-09: the images volume is **89% full, 24 GiB free**,
while a `playable` entry is expected to cost order 1.5-2.5 GB. Six entries
(3 offsets x 2 modes) would consume roughly half the remaining space. A cache
that silently fills the disk would take the product down — a direct violation of
*stability > speed*.

Rules:

- **Free-space floor.** Before a bake, require the entry's projected size plus a
  **10 GiB reserve** to be available. Below the floor, skip the bake entirely and
  run as today. A launch is never failed or delayed over cache housekeeping, and
  the engine never competes with the user for the last of their disk.
- **LRU eviction, bounded by count and bytes.** Keep at most `warm.max_entries`
  (default 4) and `warm.max_bytes` (default 8 GiB), evicting least-recently-used
  entries first. `meta.json` carries a `last_used` timestamp, stamped on each
  restore.
- **Eviction is safe by construction.** An entry is a pure derived artifact:
  deleting one costs a single cold boot, never data. Entries in use by a running
  instance are never evicted.
- **Orphan reclaim.** `prune()` (§6.1) removes entries whose base version or
  offset no longer exists, and runs from `reconcile_runtime()` — so a base update
  reclaims the space its stale entries held rather than accumulating alongside
  the new ones.
- Both limits are configurable in `configs/paths.json` under `warm`, and
  `omnidroid footprint` reports cache size and per-entry last-used.

## 6. Components

### 6.1 `omnidroid/warmcache.py` (new)

Owns the cache and nothing else. Pure functions, unit-testable with no QEMU:

- `cache_key(cfg, acct, mode, accel, qemu_version) -> str`
- `entry_path(images_dir, key) -> Path`
- `lookup(...) -> Entry | None` — validates `meta.json` and file presence;
  any mismatch returns `None` (miss, never an error)
- `begin_bake()` / `commit_bake()` / `discard_bake()` — bake into a temp dir,
  atomic rename on commit, so a partial entry is never visible
- `prune(cfg)` — reclaim orphaned entries; called from `reconcile_runtime()`
- `has_room(cfg, projected_bytes) -> bool` — free-space floor check (§5.4)
- `evict_lru(cfg)` — enforce `max_entries` / `max_bytes`, skipping entries in
  use by a running instance (§5.4)

### 6.2 `omnidroid/qemu_proc.py`

`qemu_command_arm()` and `qemu_command()` (x86 — this is not arm-only) gain a
`warm` parameter:

- **Restore:** system/data point at the entry's warm overlays (still
  `snapshot=on` — verified to leave the entry byte-identical), efivars copied
  from the entry, `-incoming defer` appended (see §2b(a)), mem/smp forced from
  meta. The load is then driven over QMP, not by the command line.
- **Bake:** real writable overlays under a temp bake dir instead of
  `snapshot=on`, so the freeze point is persistable.
- `-rtc base=utc,clock=host` on both paths (see §7).

### 6.3 `omnidroid/engine.py` — `_ensure_booted`

```
entry = None if debug else warmcache.lookup(...)
if entry:
    spawn restore -> wait for guest liveness -> post-restore steps (§7)
    (on liveness timeout: see "poisoned entry" below)
else:
    spawn bake-mode cold boot -> wait_for_boot -> post_boot
        -> bake (§6.4) -> post-restore steps (§7)
```

**Guest liveness after restore** means adb reachable *and*
`sys.boot_completed == 1` — the latter is already true in the restored state,
so this is a reachability check, not a boot wait. Budget: `RESTORE_TIMEOUT`,
30 s (generous; a healthy restore is seconds).

**Poisoned entry.** A restore that never reaches liveness within
`RESTORE_TIMEOUT` is not recoverable in place — the state file itself is
suspect. The engine then: kills the QEMU process, **deletes the cache entry**,
logs why, and falls back to a normal cold boot, which re-bakes a fresh entry.
The user sees one slow launch, never a failure. This keeps §4's spine intact
for runtime failures as well as lookup failures.

### 6.4 Bake flow

The bake runs **at the ready point, before `deliver_session`**. That ordering is
load-bearing: it is what guarantees the golden state contains no cookie, no
account, and a Roblox that has never been launched.

```
QMP stop -> migrate file:<tmp>/state -> poll until completed -> cont
```

`cont` is verified to resume the same VM, so the launch that paid for the cold
boot continues normally and the user loses nothing. A bake failure discards the
temp entry, logs, and continues the launch — the next start simply cold-boots
again.

## 7. Post-restore step order (the "must not break" list)

1. **Clock resync — first, before anything touches Roblox.** A restored guest
   wakes with its wall clock frozen at bake time; measured skew equals the wall
   time since the bake. `-rtc base=utc,clock=host` **does not** correct it
   (verified). The fix that works is an explicit guest set —
   `adb shell su -c "date -s @<host_epoch>"` — which brought skew to 0 s in the
   spike. Skipping this breaks TLS certificate validation and cookie
   acceptance, which would present as "auto-login stopped working".
2. `_enforce_hiding` — unchanged, idempotent, already per-boot.
3. `assert_kiosk_game` — unchanged.
4. Mode tuning (quality / zram / squeeze / balloon / cpuset pin) — unchanged.
5. `deliver_session` — **unchanged**.

Because the golden state is account-free, every account restores the same clean
machine and the kiosk injects its own cookie and place. **Multiple cookies and
auto-login therefore behave exactly as today**, and that account-agnosticism is
precisely what lets a single snapshot serve every account in both modes.

**Auto-capture** is unaffected: the recorder attaches at spawn as it does now,
and display device state rides inside the migration stream.

**omni-executor** is unaffected: adb, QMP and VNC ports are host-side,
per-instance command-line configuration, allocated exactly as today and
untouched by restore. Requirement is compatibility only — no new channel. A test
asserts a restored instance answers the same contract surface as a cold-booted
one.

## 8. Cross-platform

One code path everywhere; per-platform differences are confined to capability
detection.

- **macOS / HVF** — verified (§2).
- **Linux / KVM** — well-established migration support.
- **Windows / WHPX** — **unverified.** Spiked on the Windows host during
  Phase 1. If WHPX cannot migrate, `lookup()` returns `None` there permanently
  and Windows behaves exactly as it does today. This is graceful degradation,
  not a failure mode.
- `direct-io` is enabled only when the build accepts the parameter; the
  Homebrew build does not, and works fine without it.

## 9. Farming

No new mechanism. Restore makes the **existing** KSM support (`omnidroid ksm
--on`, `pid_ksm_merged_mb`, footprint reporting) substantially more effective:
instances wake byte-identical, so pages are dedupable immediately instead of
being discovered slowly. QEMU already madvises guest RAM mergeable via
`mem-merge`. The only work here is reporting — attribute dedup to warm restore
in `footprint`.

## 10. Phase 0 — measurement

Per-stage timestamps through `start`: spawn -> `boot_completed` -> each
post-boot step -> session delivered -> Roblox foreground -> in place. Emitted
under `--json`. This is permanent instrumentation, not throwaway: it is how the
win is proven and how the mechanism-C decision is later settled.

## 11. Testing

**Unit (no QEMU):** free-space floor blocks a bake and leaves behaviour
unchanged; LRU eviction respects `max_entries`/`max_bytes` and never evicts an
entry in use; key stability and sensitivity (base version bump, offset
change, mode change, mem/smp change, QEMU version change each produce a
different key); `lookup` returns `None` on missing file, corrupt `meta.json`,
and version mismatch; `begin`/`commit`/`discard` never leave a partial entry
visible.

**Integration:** cold -> bake -> restore; assert `boot_completed`, guest clock
within tolerance of host, cookie delivered and account logged in, place joined,
CLI contract surface answers on the restored instance.

**Concurrency (§8b):** a second launch against an entry already in use
cold-boots instead of restoring; a restored instance and a cold-booted one run
side by side with both reachable over adb (currently unverified — must be
tested before the interim rule is trusted).

**Regression:** `--debug` neither reads nor writes the cache; a base version
bump misses; a different offset misses; a bake failure still yields a working
instance; a corrupt/poisoned `state` file is deleted and the launch falls back
to a cold boot that succeeds; two concurrent instances restore from one entry
without interfering.

## 12. Out of scope (YAGNI)

- Mechanism C (reflink + mmap restore) — §3.
- Warm-Roblox freeze (freezing with Roblox already running at its sign-in
  screen) — deliberately deferred; revisit only if Phase 0 shows Roblox cold
  start alone makes 15 s unreachable (§13).
- Any new omni-executor command channel — compatibility only.
- Per-instance identity randomization — instances already share identity today
  (all ephemeral instances boot the same `/data` template), so restore
  introduces no regression.

## 13. Expected result and honest caveats

**Measured, not estimated (§2b):** the Android half falls from **19.5-20.3 s to
2.8-6.2 s** on the real guest. That part of the goal is demonstrated, not
promised.

Three caveats stand between that and "done":

1. **The Roblox half is still unmeasured.** Whether the end-to-end `start` ->
   in-place target of 15 s is met depends on Phase 0's number. If Roblox's own
   cold start exceeds roughly 12 s, 15 s is not reachable by this design alone
   and the deferred warm-Roblox freeze becomes necessary. The option stays open.
2. **Concurrent restore is blocked (§8b).** Until root-caused, only one instance
   per golden entry gets the fast path; siblings cold-boot. `playable`'s common
   single-instance case gets the full win now; farming does not.
3. **Windows/WHPX is unverified (§8).** It degrades to today's behaviour.
