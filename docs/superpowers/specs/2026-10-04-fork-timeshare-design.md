# A fork child that lives beside its parent: time-sharing the one memory

`fork` today is a `vfork`: the child runs in its parent's memory, every other task of the parent is
frozen, and the forking thread waits until the child executes a program or exits
(`crates/omni-linux/src/fork.rs`, spec `2026-09-27-fork-exec-design.md`). That covers the shape the
boot needs -- *fork, a few calls, `execve` or `_exit`* -- and nothing else, by design.

**Clash of Clans does not have that shape.** `libsupercell_clashofclans.so` forks an anti-tamper
watchdog: two pipes, then a child that never executes a program and blocks reading 4 bytes its
parent must write. The parent is frozen until the child finishes; the child waits for the frozen
parent. The app deadlocks in `clone`, and after 60 s ActivityManager kills it:

```
[threads] pid 404000: 11 waiting
  3000 ""                read  for 68s, lr libsupercell_clashofclans.so+0x5f18c0
  404000 "ll.clashofclans" clone for 68s
E/ActivityManager: ANR in com.supercell.clashofclans
E/ActivityManager: Reason: Process ... failed to complete startup
```

`tests/fixtures/forkwatch.c` is that shape in twenty lines, and
`fork_exec::a_forked_child_that_waits_on_its_parent_lives_beside_it` hangs on it today.

## The constraint, unchanged

A guest address is a host address (D4) and every process of an instance lives in one host process,
so **parent and child cannot both have their own content at the same addresses at the same time**.
A real `fork` -- two address spaces -- would mean the child in a host process of its own, which is
process checkpoint/restore and is not this change (see "Not here").

## The decision

**One side is resident at a time, and the memory is time-shared between them.** Each side keeps its
own view; only one view is in the address space at any moment. A side runs guest code, and the
kernel copies to and from its memory, only while it is resident. When the resident side blocks on
something only the other side can supply, the other side becomes resident.

This is what `fork` promises, kept over time rather than only across the vfork window: each side
sees the fork-time memory plus its own writes, and never the other's.

### The three pieces of state

For the life of a fork pair (`ForkPair`, in `fork.rs`):

- **`image`** -- the fork-time content of every committed private writable page. This is exactly
  today's `Snapshot`, kept for the pair's life instead of for the vfork window.
- **`parent_shelf`, `child_shelf`** -- for the side that is *not* resident, the pages where it
  differs from `image`, with that side's content. The resident side's divergence is live in the
  address space.
- **`resident`** -- which side the address space currently holds.

A side's view is always `image` + its own shelf. The invariant is a definition, not a heuristic.

### A switch

To make side B resident while A is:

1. **Quiesce A.** Halt A's tasks as `freeze_others` does, and take the layout lock exclusively --
   the same lock every guest copy holds shared, so no `copy_{from,to}_user` can interleave (this is
   already why `Snapshot::restore` takes it).
2. **Shelve A.** Walk `image`; every page whose current content differs goes into `A_shelf`, and
   `image`'s content is written back. The address space now holds the fork-time image.
3. **Unshelve B.** Write `B_shelf`'s pages. The address space now holds B's view.
4. `resident = B`; release the layout lock and let B's tasks run.

Cost is one compare pass over the pair's private memory plus writes proportional to the divergence,
which is the child's working set -- for a watchdog, its stack and a few globals.

### What triggers a switch

A side asks to become resident when it is about to touch the memory and is not:

- **before guest code runs** -- `run_task`'s `state.store(IN_GUEST)`, and the existing park points
  (`process.rs:500`, after a system call, and `:1084`, on a halt). `park_if_frozen` already parks a
  task that may not re-enter guest code; being non-resident is the same thing, so it is the same
  park.
- **before any kernel copy** -- `GuestMem::check`, the one gate every `copy_{from,to}_user` passes
  (`guest.rs`), before it takes the layout lock. This is what keeps a blocked call that completes
  for the non-resident side from writing into the resident side's memory. Parent and child have
  separate `GuestMem` views over one `GuestSpace`, so the side is known there.

A side stops being resident when it is blocked and the other side wants in: the request above is
what moves residency, so a side that is running keeps it, and a side waiting in a host call holds
nothing. A watchdog child therefore costs two switches -- in to read, out again -- per exchange
with its parent, and none while it waits.

The fast path is one `Option` load: a process not in a fork pair is unchanged, which is every
process except the two, for the window the pair exists.

### The pair ends

At `execve` or the child's exit -- today's release points -- the pair dissolves: the parent becomes
resident with its shelf applied, `image` and both shelves are dropped, and `fork.rs` behaves exactly
as it does now. **The existing fork+exec path keeps today's cost and today's code**: the child is
started, the parent waits, and nothing is shelved, because the child never asks to be non-resident.
The journal of the parent's in-flight kernel writes stays as it is for that path.

## Gates

| Gate | What it proves |
|---|---|
| `fork_exec::a_forked_child_that_waits_on_its_parent_lives_beside_it` (`forkwatch`) | the parent runs on after the fork; each side's globals, heap and stack are its own across two exchanges; a parent thread keeps running; the child sees end of file and is reaped |
| `fork_exec`'s three existing tests | fork+exec, `execve` without a fork, and the shell are unchanged |
| the boot | vold's `init_user0` and installd's dexopt still fork-exec; `r_roblox` green |
| Clash of Clans | `com.supercell.clashofclans` past `bindApplication`, no ANR, its first frame on screen |

## Not here

- A fork child in a host process of its own (real concurrent fork). It is the right answer for a
  child that wants to run *at the same time* as its parent rather than take turns with it, and it
  is checkpoint/restore (`docs/research/2026-10-01-checkpoint-restore.md`). A watchdog takes turns,
  so this change is what it needs.
- `fork` from more than one thread of a process at once, and a child that forks again: one pair per
  process, and a second fork while a pair is live waits for it, as it does today.
- Shared mappings, which `fork` shares anyway and which are therefore never shelved.
