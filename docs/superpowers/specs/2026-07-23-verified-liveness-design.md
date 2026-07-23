# Verified Liveness — Design

**Date:** 2026-07-23
**Status:** Approved (brainstorming) — ready for implementation plan
**Thread:** A of the omnidroid improvement decomposition (A verified-liveness, B carve engine.py, C density/scale, D daemon+observability)

## Problem

Omnidroid decides whether an emulator instance is alive purely from a pid. Each
instance is tracked by `runtime/<name>/run.json` (`pid`, `adb_port`,
`qmp_port`, `vnc_port`, `base`, `started`, optional `reserving`), and liveness
is a single predicate `pid_alive(pid)` (an `os.kill(pid, 0)` / `waitpid` check).
Nothing ties that pid to *the QEMU that is actually running on those ports*.

This is the root cause of the open stale-QEMU detection bug: a live QEMU can be
reported as stopped, its port slot freed and re-issued, and a new `start` then
attaches to the still-running old instance (tell: "boot completed after 0.0
min").

### Three failure modes, all from pid-only trust

1. **PID recycle → false "alive."** The recorded pid is dead; the OS reissues
   that pid to an unrelated process. `pid_alive` returns True → the slot is
   wrongly held and a ghost instance appears in `list`.
2. **False "dead" → the reported bug.** `pid_alive` returns False for a *live*
   QEMU (reparent/`waitpid` edge, or a transiently unreadable run.json). The
   slot is freed while the QEMU is alive → `allocate_ports` re-issues the port
   → a new `start` connects to the old live VM → "boot completed after 0.0 min."
3. **Orphan QEMU.** QEMU is alive but its run.json is lost (crash between spawn
   and write, or a manual wipe). It is invisible to the manager while its ports
   are silently in use → the next allocation collides.

### Affected code (current)

- `pid_alive(pid)` — the sole liveness predicate.
- `running_instances()` — lists live instances; uses `pid_alive`.
- `_claimed_port_indices()` — computes claimed ports; uses `pid_alive`.
- `running_pid(name)` — per-name liveness; uses `pid_alive`.
- `allocate_ports(cfg)` — picks the lowest free port index from claimed slots.
- `spawn_qemu(...)` / `qemu_command*(...)` — build the QEMU command and write
  the real pid into run.json.

## Principle

**The running QEMU is the source of truth, not `run.json`.** The QMP monitor
socket is an authoritative liveness *and* identity oracle that we already have
(`qmp()` helper) but do not use for identity. run.json becomes a *claim* that
must be confirmable against the live process.

## Design

Four layers. Layers 1–2 fix identity; layer 3 is the by-construction guarantee;
layer 4 keeps bookkeeping honest.

### 1. Stamp identity at spawn

Add `-name guest=omnidroid-<name>` to the QEMU command in `qemu_command` /
`qemu_command_arm`. Record the same token in run.json as a new `identity` field
(alongside `pid`). One mechanism, readable two ways:

- Linux: `/proc/<pid>/cmdline` contains the `-name` token.
- Any platform: QMP `query-name` returns the guest name.

No `-uuid` — the name token is sufficient; a second mechanism is redundant.

### 2. `instance_live(rec) -> bool` replaces bare `pid_alive` at the three call sites

New predicate used by `running_instances`, `_claimed_port_indices`, and
`running_pid`. Two checks, cheap-first:

- **Cheap path (Linux):** `pid_alive(pid)` **and** `/proc/<pid>/cmdline`
  contains the expected `-name` token. Kills PID-recycle false positives (mode
  1) with no socket — important on the 120-VM farm.
- **Authoritative / portable path:** open `qmp_port`, send `query-name`, compare
  the returned token to the expected one. Socket refused → dead; token mismatch
  → not our instance.

Combination rule: if the cheap pid+cmdline check affirms "our QEMU," trust it
and skip the socket (a busy monitor socket must not flap a live instance to
"dead"). Otherwise fall to the QMP probe.

Records lacking an `identity` field (pre-upgrade instances) fall back to
pid-only liveness with a one-line warning; they reconcile on next stop/restart.

### 3. Probe-before-allocate (the safety net)

`allocate_ports` performs a final connect-probe on the candidate qmp/adb port
before handing the slot out: if anything answers, skip the index. Even when
bookkeeping is wrong, we never issue a port a live QEMU answers on. This makes
failure modes 2 and 3 *impossible by construction*, not merely less likely.

Runs inside the existing `_launch_lock()` critical section, so the
probe→reserve step stays atomic across concurrent launches.

### 4. `reconcile_runtime()` self-heal sweep

Synchronous sweep triggered by existing commands (`start`, `list`). For each
`runtime/<name>`:

- Dead pid **and** silent port → GC the directory.
- run.json missing but a QEMU answers on a known port → surface as an orphan
  (log/report). **Do not auto-adopt** — auto-adoption of a stray QEMU is risky
  and rare; the operator can `stop` it.

No new daemon or background reaper: reconciliation is synchronous, driven by the
commands users already run.

## Error handling

The QMP probe is fast and fail-safe (it runs in hot paths: `list`, every
`allocate_ports`). Short connect timeout (~250ms). Every probe failure is
interpreted in the **safe direction**:

- In `instance_live`: probe error/ambiguous → treat as dead *only* when the
  cheap pid+cmdline check also fails; if pid+cmdline affirms our QEMU, trust it
  and skip the socket.
- In `allocate_ports`: probe error → treat the port as occupied (skip it).
  Ambiguity costs at most one port index — never a collision.

## Cross-platform

- **Linux** (the 120-VM farm): cheap cmdline path + QMP probe.
- **macOS** (dev host, no `/proc`): degrade to `pid_alive` + QMP `query-name`.
- **Windows** (single-launch product): `pid_alive` + optional QMP probe.

The identity token is universal; only the `/proc/<pid>/cmdline` shortcut is
Linux-gated, matching the existing `IS_WINDOWS` branching in `pid_alive`.

## Backward compatibility / rollout

`run.json` gains an `identity` field. Records without it fall back to pid-only
liveness with a warning and reconcile on next `stop`/restart. `runtime/` is
throwaway (wiped on stop), so the fleet drains cleanly as instances cycle — new
spawns stamp identity immediately. No migration script.

## Test plan

Extends the existing pytest suite; no live QEMU required.

- **Regression lock for mode 2:** write a run.json whose `pid` points at a live
  *unrelated* process (spawn a `sleep`), with no matching cmdline/QMP identity.
  Assert the old predicate reports "running" while `instance_live` reports "not
  ours."
- **Fake QMP server** (localhost socket returning a greeting + `query-name`):
  `instance_live` returns True on a matching token, False on mismatch;
  `allocate_ports` skips a port that answers.
- **Orphan (mode 3):** QMP answers on a port with no run.json →
  `reconcile_runtime` flags it; `allocate_ports` avoids it.
- **Cheap-path unit test:** cmdline match/mismatch via a stubbed `/proc` reader.
- **Suite stays green:** behavior-preserving at the CLI surface.

## Scope / YAGNI

- Identity = `-name` token only. No `-uuid`.
- `reconcile_runtime` reports/GCs orphans; it does **not** auto-adopt a stray
  QEMU back under management.
- No new daemon / background reaper; reconciliation is synchronous.

## Out of scope (other threads)

Carving `engine.py` (B), density/scale (C), and a daemon+observability layer (D)
are separate specs. This thread is deliberately contained to identity/liveness.
