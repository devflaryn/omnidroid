# 4 KiB Guest Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every process on the real-AOSP path runs with 4 KiB pages on any host, so Roblox
2.740.931's 4 KiB-only `libzstd-jni` works on the M1's 16 KiB host and the PS99 gate passes.

**Architecture:**
* A per-space sub-page overlay in `omni-mem`'s `GuestSpace` tracks the 4 KiB parts of host pages
  that the guest maps or protects in pieces. A host page gets the least permissive protection its
  mapped parts need.
* An access the host refuses but the guest view allows reaches the slow path. The fault path is
  already built: macOS Mach thread port → pager declines → dynarmic callback. The slow path serves
  the access through a read-write alias of that host page (`mach_vm_remap`).
* The overlay is opt-in per space (only `omni-linux` opts in) and inert when the host page is
  4 KiB.

**Tech Stack:** Rust (workspace crates `omni-platform`, `omni-mem`, `omni-cpu`, `omni-linux`),
dynarmic (arm64 backend, unchanged in M1), Mach VM APIs on macOS.

**Spec:** `docs/superpowers/specs/2026-09-29-4k-guest-pages-design.md` (read it first; this plan
argues from it).

## Global Constraints

* Guest page is `4096` (`omni_mem::GUEST_PAGE`). Host page is `omni_platform::vm::page_size()`,
  read at run time. Never write `16384` or `0x4000` in code.
* The overlay is active only when all three hold: `GuestSpaceConfig::guest_page == Some(4096)`,
  `vm::page_size() > 4096`, and `vm::supports_alias()`. Otherwise every `GuestSpace` method behaves
  byte for byte as before.
* Windows, x86-64 Linux and the direct path (`omni-android`, `omnidroid play`) do not change
  behaviour.
* No dynarmic patch in M1. `crates/dynarmic-sys/vendor` is untouched.
* Commits:
  * small and on `mac-port`, one per task (a task may have more);
  * message style `type(scope): sentence`, as in `git log`;
  * every message ends with the two attribution lines from the session (`Co-Authored-By: Claude
    Opus 5.5 <noreply@anthropic.com>`, `Claude-Session: ...`).
* Multi-line scripts are written with the Write tool, never a long heredoc.
* On macOS in a non-interactive shell: `. ~/.cargo/env` before `cargo` if `cargo` is not found.
* Test commands run from the repo root: `/Users/berat/Desktop/Omni Apps/omnidroid` (quote the
  path; it has a space).

## Review Focus

1. **A syscall buffer spanning a split and an uncommitted lazy page** (`read(fd, buf, 32 KiB)` into a
   fresh stack or heap). The copy must succeed chunk by chunk and must not fault in host code.
   Test: Task 8, `a_read_into_a_buffer_across_a_split_page_lands_whole`.
2. **`MAP_FIXED` over a range whose edge host pages are split and belong to other mappings.** The
   neighbours' bytes and protections must survive.
   Test: Task 4, `a_fixed_map_over_part_of_a_split_page_keeps_the_neighbours`.
3. **`mprotect` across a hole** (a range that includes a `Hole` part). Linux answers `ENOMEM` and
   changes nothing. Test: Task 4, `protect_over_a_hole_is_enomem_and_changes_nothing`.
4. **An `mremap` that moves a range with split edges.** The content and each part's protection must
   arrive intact. Test: Task 8, `mremap_moves_split_edges_intact`.
5. **Many threads faulting on one split page at once** must neither lose updates nor spin in the
   pager. Test: Task 7, `four_threads_increment_a_split_page_counter`.

---

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `crates/omni-platform/src/vm/mod.rs` | modify | `supports_alias`, `alias`, `unalias` (public seam) |
| `crates/omni-platform/src/vm/macos.rs` | modify | `mach_vm_remap`-based alias |
| `crates/omni-platform/src/vm/{linux,windows}.rs` | modify | `supports_alias() == false`; `alias`/`unalias` return `Unsupported` |
| `crates/omni-platform/tests/vm_alias.rs` | create | probe: alias semantics on this host |
| `crates/omni-mem/src/subpage.rs` | create | pure overlay logic: `Part`, `Split`, host protection, runs; `SubPages` state |
| `crates/omni-mem/src/space.rs` | modify | config field, activation, guest-page ops, guest view, `access_ptr`, chunking, strict gaps, stats |
| `crates/omni-mem/src/pager.rs` | modify | decline split pages at once; `split_declined` stat |
| `crates/omni-mem/src/lib.rs` | modify | exports (`GUEST_PAGE`, `AccessPtr`, `SplitStats`) |
| `crates/omni-mem/tests/subpage.rs` | create | overlay behaviour tests (pass trivially on 4 KiB hosts) |
| `crates/omni-cpu/src/dynarmic/callbacks.rs` | modify | `fetch`/`data_ptr` via `access_ptr`; straddle; served counter |
| `crates/omni-cpu/src/dynarmic/mod.rs` | modify | slice invariant subtracts served split accesses |
| `crates/omni-cpu/tests/harness/mod.rs` | modify | `Guest::with_space(GuestSpaceConfig)` |
| `crates/omni-cpu/tests/subpage.rs` | create | guest code against split pages |
| `crates/omni-linux/src/process.rs` | modify | `reserve_space` asks for `guest_page: Some(GUEST_PAGE)` |
| `crates/omni-linux/src/mm.rs` | modify | page = guest page; placement; file congruence; drop `protect_widened`; strict gaps |
| `crates/omni-linux/src/guest.rs` | modify | `check` → chunked access through `access_ptr` |
| `crates/omni-linux/src/pagecompat.rs`, `lib.rs`, `fd.rs` | delete / modify | remove relayout |
| `crates/omni-linux/tests/mm.rs` | modify | 4 KiB-exact expectations |
| `crates/omni-linux/tests/page_size.rs` | create | `getconf PAGESIZE`, `AT_PAGESZ` through real bionic |
| `crates/omni-linux/tests/c2_apk_in_app_process.rs` | modify | `libzstd-jni` back in, layout assertion |
| `docs/ports/macos.md`, `docs/DECISIONS.md` | modify | D42: 4 KiB guest pages |

---

### Task 0: Pin the `libzstd-jni` failure (time-boxed, about 2 h, no product code)

**Files:**
- Create: `docs/research/2026-09-29-libzstd-jni-16k.md`

- [ ] **Step 1: Run the current build with the app's syscalls traced.**

```sh
OMNI_TRACE_APP=com.roblox.client OMNI_SIGNAL_TRACE=1 tools/aosp_play.sh \
  --apk ~/Desktop/Roblox-2.740.931.apk --cookie ~/Desktop/cookies/HezMi_ImYu916.txt \
  --place 8737899170 --minutes 3 > "$SCRATCH/diag1.log" 2>&1
```

The full log is `$TMPDIR/omni-linux-r-<pid>.log`.

- [ ] **Step 2: Extract the evidence with a script, not by reading the log.** Collect:
  * `libzstd-jni`'s load base from the `mmap` of its path;
  * every later `mmap`, `mprotect` and `munmap` inside `[base, base + size)`;
  * any `openat` or `read` of `/proc/self/maps`;
  * the fatal signal (`si_addr`, pc, lr) and whether pc and lr are inside the library.
- [ ] **Step 3: Disassemble around the faulting pc and its caller.** The library is taken from the
  APK and its bytes are dumped after unpacking, if the log names the plaintext range. Work out
  which table was not set up and what fills it.
- [ ] **Step 4: Write the note.** It records the measured facts, the mechanism as far as it is
  established, and what a 4 KiB guest changes about it. If the mechanism can be checked without the
  whole app, add that check to Task 10's test. If the time box runs out, the note says so and the
  gate is the proof.
- [ ] **Step 5: Commit** `docs(research): what libzstd-jni does on a 16 KiB guest`.

---

### Task 1: `omni-platform` alias primitive and its probe

**Files:**
- Modify: `crates/omni-platform/src/vm/mod.rs`, `vm/macos.rs`, `vm/linux.rs`, `vm/windows.rs`
- Test: `crates/omni-platform/tests/vm_alias.rs`

**Interfaces:**
- Produces:
  - `pub fn supports_alias() -> bool`
  - `pub unsafe fn alias(src: *mut u8, dst: *mut u8, len: usize) -> VmResult<()>`: `dst`
    becomes the same memory as `src`, read-write, replacing whatever was at `dst` (which must be
    inside a reservation the caller owns).
  - `pub unsafe fn unalias(dst: *mut u8, len: usize) -> VmResult<()>`: `dst` becomes
    inaccessible reserved space again.

  Both need page-aligned `src`, `dst` and `len`.

- [ ] **Step 1: Write the failing probe test** (`tests/vm_alias.rs`):

```rust
//! The alias primitive the 4 KiB guest overlay stands on (`omni-mem`'s `subpage`): two host
//! addresses, one memory. Pinned here before anything depends on it.
use omni_platform::vm::{self, Protection};

fn page() -> usize {
    vm::page_size()
}

/// A committed read-write page, and an inaccessible reserved page to alias it at.
fn pair() -> (*mut u8, *mut u8, vm::Reservation, vm::Reservation) {
    let src = vm::reserve(page(), page()).expect("reserve src");
    let dst = vm::reserve(page(), page()).expect("reserve dst");
    unsafe { vm::commit(src.base() as *mut u8, page(), Protection::ReadWrite).expect("commit src") };
    (src.base() as *mut u8, dst.base() as *mut u8, src, dst)
}

#[test]
fn an_alias_is_the_same_memory_both_ways_whatever_the_source_allows() {
    if !vm::supports_alias() {
        return;
    }
    let (src, dst, _s, _d) = pair();
    unsafe {
        src.write(0x11);
        vm::alias(src, dst, page()).expect("alias");
        assert_eq!(dst.read(), 0x11, "the source's byte through the alias");
        dst.add(1).write(0x22);
        assert_eq!(src.add(1).read(), 0x22, "the alias's byte through the source");
        for p in [Protection::Read, Protection::None] {
            vm::protect(src, page(), p).expect("protect src");
            dst.add(2).write(0x33); // must not fault: the alias stays read-write
            vm::protect(src, page(), Protection::ReadWrite).expect("restore src");
            assert_eq!(src.add(2).read(), 0x33, "{p:?}");
            src.add(2).write(0);
        }
        vm::unalias(dst, page()).expect("unalias");
        assert_eq!(src.read(), 0x11, "unaliasing leaves the source alone");
    }
}

#[test]
fn a_fresh_mapping_over_the_source_detaches_the_alias() {
    if !vm::supports_alias() {
        return;
    }
    let (src, dst, _s, _d) = pair();
    unsafe {
        src.write(0x44);
        vm::alias(src, dst, page()).expect("alias");
        // omni-mem's decommit on macOS is a fresh MAP_FIXED mapping over the range.
        vm::decommit(src, page()).expect("decommit");
        vm::commit(src, page(), Protection::ReadWrite).expect("recommit");
        assert_eq!(src.read(), 0, "recommitted memory is zero");
        assert_eq!(dst.read(), 0x44, "the alias still holds the old memory: callers must re-alias");
    }
}

#[test]
fn no_alias_where_the_host_page_is_the_guest_page() {
    if page() == 4096 && !cfg!(target_os = "macos") {
        assert!(!vm::supports_alias(), "a 4 KiB host never needs one");
    }
}
```

- [ ] **Step 2: Run it and confirm it fails to compile.**
  Run: `cargo test -p omni-platform --test vm_alias`
  Expected: `cannot find function supports_alias in module vm`.
- [ ] **Step 3: Implement.**
  * In `vm/mod.rs`, add the three functions. Each delegates to `imp::` (the platform module
    alias the file already uses) and checks page alignment first, returning the file's existing
    misalignment error.
  * In `vm/macos.rs`, next to the existing `mach_vm_allocate` extern block:

```rust
extern "C" {
    fn mach_vm_remap(
        target_task: MachPort, target_address: *mut u64, size: u64, mask: u64, flags: i32,
        src_task: MachPort, src_address: u64, copy: i32, cur_protection: *mut i32,
        max_protection: *mut i32, inheritance: u32,
    ) -> KernReturn;
}
const VM_FLAGS_FIXED: i32 = 0x0000;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const VM_INHERIT_NONE: u32 = 2;

pub(super) fn supports_alias() -> bool {
    true
}

pub(super) fn alias(src: usize, dst: usize, len: usize) -> VmResult<()> {
    let mut at = dst as u64;
    let (mut cur, mut max) = (0i32, 0i32);
    // SAFETY: both ranges are page-aligned and owned by the caller; `copy = 0` shares the memory.
    let kr = unsafe {
        mach_vm_remap(mach_task_self(), &mut at, len as u64, 0, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                      mach_task_self(), src as u64, 0, &mut cur, &mut max, VM_INHERIT_NONE)
    };
    if kr != 0 || at != dst as u64 {
        return Err(/* the file's existing kern-return error constructor, operation "alias" */);
    }
    protect(dst, len, Protection::ReadWrite)
}

pub(super) fn unalias(dst: usize, len: usize) -> VmResult<()> {
    // A fresh inaccessible anonymous mapping over it, as `decommit_to_placeholder` does.
    decommit_to_placeholder(dst, len)
}
```

  Use the file's existing `mach_task_self` declaration and error helpers; read the top of
  `macos.rs` for their names.
  * In `linux.rs` and `windows.rs`: `supports_alias() -> false`, and `alias`/`unalias` return the
    crate's `Unsupported`-kind error (use whichever variant `vm/error.rs` has for an operation the
    platform lacks).
- [ ] **Step 4: Run and confirm it passes.**
  Run: `cargo test -p omni-platform --test vm_alias`
  Expected: 3 passed on the M1.
  **If `a_fresh_mapping_over_the_source_detaches_the_alias` fails the other way** (the alias
  follows), keep the test but flip its assertion. Record the fact in the module docs: Task 4's
  re-aliasing then becomes a no-op safety net.
- [ ] **Step 5: Commit** `feat(platform): a host page mapped twice (vm::alias) on macOS`.

---

### Task 2: `omni-mem::subpage`, the pure overlay logic

**Files:**
- Create: `crates/omni-mem/src/subpage.rs`
- Modify: `crates/omni-mem/src/lib.rs` (`mod subpage; pub use subpage::{GUEST_PAGE, Part};`), and
  `space.rs:353`: `pub const SMALL_PAGE` becomes `pub use crate::subpage::GUEST_PAGE as SMALL_PAGE;`
  so the old name keeps compiling.

**Interfaces:**
- Produces:

```rust
pub const GUEST_PAGE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Mapped, with the protection the guest asked for.
    Mapped(Protection),
    /// Not mapped; an access here may succeed (lenient, the default).
    Hole,
    /// Not mapped, and must fault: counts as `Protection::None` on the host.
    StrictHole,
}

/// One host page's parts. Only pages whose parts differ are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split { pub parts: Vec<Part> }

impl Split {
    pub fn new(parts_per_page: usize, fill: Part) -> Self;
    /// `Some(p)` when every part is `Mapped(p)`: the page is uniform and leaves the overlay.
    pub fn uniform(&self) -> Option<Protection>;
    /// Every part is a hole (of either kind): the host page can be unmapped.
    pub fn empty(&self) -> bool;
    /// The host protection: read/write intersection over `Mapped` parts and `StrictHole`s
    /// (as `None`); `Hole`s do not count; execute is never needed on the host.
    /// `None` if the page has no counted part at all.
    pub fn host_protection(&self) -> Protection;
    /// Whether some `Mapped` part allows an access the host protection refuses: the page traps
    /// and needs an alias.
    pub fn traps(&self) -> bool;
    /// The run of equal parts containing part `i`: (first index, count, part).
    pub fn run_at(&self, i: usize) -> (usize, usize, Part);
    /// Set parts `[from, to)` to `part`.
    pub fn set(&mut self, from: usize, to: usize, part: Part);
}
```

- [ ] **Step 1: Write the failing unit tests** at the bottom of `subpage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use Protection::*;

    fn split(parts: &[Part]) -> Split {
        Split { parts: parts.to_vec() }
    }

    #[test]
    fn host_protection_is_the_least_any_counted_part_allows() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).host_protection(), Read);
        assert_eq!(split(&[Part::Mapped(ReadExecute), Part::Mapped(ReadWrite)]).host_protection(), Read);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Mapped(None)]).host_protection(), None);
        assert_eq!(split(&[Part::Mapped(ReadWriteExecute), Part::Mapped(ReadWrite)]).host_protection(), ReadWrite);
    }

    #[test]
    fn a_lenient_hole_does_not_count_and_a_strict_one_does() {
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::Hole]).host_protection(), ReadWrite);
        assert_eq!(split(&[Part::Mapped(ReadWrite), Part::StrictHole]).host_protection(), None);
        assert_eq!(split(&[Part::Hole, Part::Hole]).host_protection(), None);
    }

    #[test]
    fn a_page_traps_only_when_a_mapped_part_allows_more_than_the_host() {
        assert!(!split(&[Part::Mapped(ReadWrite), Part::Hole]).traps());
        assert!(split(&[Part::Mapped(ReadWrite), Part::Mapped(Read)]).traps());
        assert!(!split(&[Part::Mapped(None), Part::StrictHole]).traps());
        // Execute alone never traps: guest code is fetched in software.
        assert!(!split(&[Part::Mapped(Read), Part::Mapped(ReadExecute)]).traps());
    }

    #[test]
    fn uniform_and_empty() {
        assert_eq!(split(&[Part::Mapped(Read), Part::Mapped(Read)]).uniform(), Some(Read));
        assert_eq!(split(&[Part::Mapped(Read), Part::Hole]).uniform(), Option::None);
        assert!(split(&[Part::Hole, Part::StrictHole]).empty());
        assert!(!split(&[Part::Hole, Part::Mapped(None)]).empty());
    }

    #[test]
    fn runs_and_set() {
        let mut s = Split::new(4, Part::Hole);
        s.set(1, 3, Part::Mapped(ReadWrite));
        assert_eq!(s.run_at(0), (0, 1, Part::Hole));
        assert_eq!(s.run_at(2), (1, 2, Part::Mapped(ReadWrite)));
        assert_eq!(s.run_at(3), (3, 1, Part::Hole));
    }
}
```

- [ ] **Step 2: Run and confirm they fail.**
  Run: `cargo test -p omni-mem --lib subpage`
  Expected: compile errors (`Split` not found).
- [ ] **Step 3: Implement.** Bits: R = readable, W = writable.
  * `host_protection`: fold the counted parts. A `StrictHole` and a `Mapped(None)` count as
    neither R nor W. The result is R if every counted part is readable, and R+W if every counted
    part is also writable. It maps to `None`, `Read` or `ReadWrite`, and is `None` when nothing is
    counted.
  * `traps`: some `Mapped(p)` has R or W that `host_protection()` lacks.
  * `uniform`: every part is `Mapped(p)` with the same `p`.
  * `empty`: every part is `Hole` or `StrictHole`.
  * `run_at`: scan left and right from `i` while the parts are equal.
  * `set`: fill.

  Module docs state invariants 1-4 from the spec's Unit 2 in two short paragraphs.
- [ ] **Step 4: Run and confirm they pass.**
  Run: `cargo test -p omni-mem --lib subpage`
  Expected: 5 passed.
- [ ] **Step 5: Commit** `feat(mem): the 4 KiB parts of a host page, and what the host must allow`.

---

### Task 3: Opting a space in: `GuestSpaceConfig::guest_page` and `guest_page_size()`

**Files:**
- Modify: `crates/omni-mem/src/space.rs`:
  * the `GuestSpaceConfig` struct at 244 and its `Default` (grep `impl Default for GuestSpaceConfig`);
  * `build` at 577;
  * the struct at 470.
- Create: `crates/omni-mem/tests/subpage.rs`

**Interfaces:**
- Produces:
  - `GuestSpaceConfig::guest_page: Option<usize>` (default `None`).
  - `GuestSpace::guest_page_size(&self) -> usize`: `GUEST_PAGE` when the overlay is active,
    otherwise `page_size()`.
  - `GuestSpace::subpages_active(&self) -> bool`.
  - A private field `sub: Option<SubPagesHandle>` on `GuestSpace`, where
    `SubPagesHandle { count: AtomicUsize, bits: OnceLock<Box<[AtomicU64]>>, state: Mutex<SubPages> }`.
    `SubPages` (in `subpage.rs`) is
    `{ parts_per_page: usize, split: BTreeMap<GuestAddr, Split>, alias: Option<Reservation>, strict: Vec<(GuestAddr, GuestAddr)>, served: HashMap<GuestAddr, u64>, served_total: u64 }`.
  - `GuestSpace::is_trapping(&self, host_page: GuestAddr) -> bool`: lock-free. `count == 0`
    returns false with no further load; otherwise it tests the bit `(host_page - base) / page`.

- [ ] **Step 1: Write the failing test** in `tests/subpage.rs`:

```rust
//! The 4 KiB guest overlay (spec 2026-09-29-4k-guest-pages): on a host whose page is larger and
//! that can alias, a space that asks for 4 KiB pages gets them; everywhere else nothing changes.
use omni_mem::{GuestSpace, GuestSpaceConfig, GUEST_PAGE};
use omni_platform::vm;

pub fn space(guest_page: Option<usize>) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, guest_page, ..GuestSpaceConfig::default() })
        .expect("a space")
}

fn overlay_expected() -> bool {
    vm::page_size() > GUEST_PAGE && vm::supports_alias()
}

#[test]
fn a_space_that_asks_gets_4_kib_pages_where_the_host_can_give_them() {
    let s = space(Some(GUEST_PAGE));
    assert_eq!(s.subpages_active(), overlay_expected());
    assert_eq!(s.guest_page_size(), if overlay_expected() { GUEST_PAGE } else { s.page_size() });
}

#[test]
fn a_space_that_does_not_ask_is_as_before() {
    let s = space(None);
    assert!(!s.subpages_active());
    assert_eq!(s.guest_page_size(), s.page_size());
}
```

- [ ] **Step 2: Run and confirm it fails.**
  Run: `cargo test -p omni-mem --test subpage`
  Expected: `no field guest_page`.
- [ ] **Step 3: Implement.**
  * Add the field, documented as in the spec.
  * In `build`, validate it: `Some(p)` with `p != GUEST_PAGE` is `InvalidConfig { field: "guest_page", reason: "only 4096 is supported" }`.
  * Set `sub = (config.guest_page.is_some() && page > GUEST_PAGE && vm::supports_alias()).then(SubPagesHandle::new)`.
  * Implement the accessors. `is_trapping` is `#[inline]`.
- [ ] **Step 4: Run it and the existing suite.**
  Run: `cargo test -p omni-mem`
  Expected: all green; the new tests are 2 passed.
- [ ] **Step 5: Commit** `feat(mem): a space may ask for 4 KiB guest pages (GuestSpaceConfig::guest_page)`.

---

### Task 4: Map, protect and unmap at 4 KiB on an active space

**Files:**
- Modify: `crates/omni-mem/src/space.rs`: `map_anonymous` (839), `protect` (1037), `unmap` (1154),
  `region_at`/`region_at_locked` (1429), `regions`/`mapped_regions` (1373), `any_executable` (1467).
- Modify: `crates/omni-mem/src/subpage.rs`: operations on `SubPages`.
- Test: `crates/omni-mem/tests/subpage.rs`

**Interfaces:**
- Consumes: `Split`, `Part`, `SubPagesHandle`, `vm::alias` and `vm::unalias` (Tasks 1-3).
- Produces (behaviour; the signatures are unchanged):
  * On an active space, `map_anonymous(Placement::Fixed(a), len, ..)`, `protect(a, len, ..)` and
    `unmap(a, len)` accept `a` and `len` that are multiples of `GUEST_PAGE`.
  * `map_anonymous(Placement::Anywhere|Hint, len, ..)` returns a host-page-aligned address. A
    `len` that is not a multiple of the host page leaves the tail parts as `Hole`s.
  * `region_at(g)` is the guest view: in a tracked host page, the run containing `g`
    (`mapping: None` and `None` returned for a hole). An untracked answer is clipped so that it
    never covers a tracked host page.
  * `mapped_regions()` and `regions()` are the guest view.
  * A private helper `fn with_writable_view(&self, inner: &mut Inner, host_page: GuestAddr, f: impl FnOnce(*mut u8))`
    uses the page's alias, or the host address if the host protection is writable, or else a
    temporary alias (made, used, unaliased).

**Algorithm** (the core; implement as `Inner::sub_apply(&mut self, sub: &mut SubPages, addr, len, op)`,
where `op` is `Map(Protection) | Protect(Protection) | Unmap`):

1. `split_at_pages(addr, len, host_page)` gives `whole`, `head` and `tail`.
2. `whole`: first drop any tracked page inside it (unalias, clear its bit, `count -= 1`, remove it
   from the `BTreeMap`), then call today's host path unchanged (`map_anonymous(Fixed)`, `protect_range`,
   `unmap`).
3. Each partial host page `P` (head or tail):
   * **Not tracked, free in the region map, op `Map(p)`:** map `P` whole as committed anonymous
     `ReadWrite` (the existing Eager path for one host page). Track `Split::new(n, Hole)`, then
     `set(parts, Mapped(p))`.
   * **Not tracked, mapped uniform with `q`:** track `Split::new(n, Mapped(q))`. Before tracking,
     if the entry is a file view (`OsState::View`): privatise it (read the host page's bytes,
     `unmap` it, map it anonymous committed `ReadWrite`, write the bytes back). If it is a lazy
     uncommitted placeholder, commit it first.
   * **Op `Map(p)`:** all target parts must be holes, or the result is `MemError::NotMapped`-style
     `AlreadyMapped` (use the variant `Fixed` placement already returns for an occupied range).
     Set them to `Mapped(p)`, and zero those bytes through `with_writable_view`.
   * **Op `Protect(p)`:** all target parts must be `Mapped(_)`; otherwise return the error today's
     `require_mapped` returns (Mm maps it to `ENOMEM`). Set them.
   * **Op `Unmap`:** set them to `StrictHole` if the range intersects `sub.strict`, else `Hole`.
   * **Then settle `P`:**
     * `uniform() == Some(p)`: untrack it and `protect_range(P, page, p)`.
     * `empty()`: untrack it and unmap `P` on the host.
     * Otherwise: `protect_range(P, page, host_protection())`. If `traps()`, ensure the alias
       exists (make the space-sized alias reservation lazily, then `vm::alias(host_addr(P), alias_base + (P - base), page)`)
       and set the bit; if not `traps()` and the bit is set, unalias and clear it. Keep `count` equal
       to the number of set bits.
4. Every step runs under the one `self.write()` guard, which bumps the generation (invariant 4).
   Order per host page: compute the new `Split` → apply to the host → publish into the map and
   bits. If the host call fails, return the error with nothing published.

- [ ] **Step 1: Write the failing tests** (append to `tests/subpage.rs`; each returns early when
  `!overlay_expected()`, after asserting on a 4 KiB host that the same calls behave as before):

```rust
use omni_mem::{CommitPolicy, Placement, Protection};

fn host_page() -> usize {
    vm::page_size()
}

/// An active space and one committed read-write host page in it.
fn one_page() -> (GuestSpace, usize) {
    let s = space(Some(GUEST_PAGE));
    let at = s.map_anonymous(Placement::Anywhere { align: host_page() }, host_page(), Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    (s, at)
}

#[test]
fn two_parts_of_one_host_page_keep_two_protections() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert_eq!(s.region_at(at).unwrap().protection, Protection::Read);
    assert_eq!(s.region_at(at).unwrap().len, GUEST_PAGE);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().protection, Protection::ReadWrite);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().start, at + GUEST_PAGE);
    assert!(s.is_trapping(at), "a read-write part under a read-only host page traps");
}

#[test]
fn a_page_made_uniform_again_leaves_the_overlay() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    s.protect(at, GUEST_PAGE, Protection::ReadWrite).unwrap();
    assert!(!s.is_trapping(at));
    assert_eq!(s.region_at(at).unwrap().len, host_page(), "one region again");
}

#[test]
fn a_4_kib_mapping_is_its_4_kib_and_the_rest_is_a_hole() {
    if !overlay_expected() { return; }
    let s = space(Some(GUEST_PAGE));
    let at = s.map_anonymous(Placement::Anywhere { align: host_page() }, GUEST_PAGE, Protection::ReadWrite, CommitPolicy::Lazy).unwrap();
    assert_eq!(at % host_page(), 0, "unhinted: host-page-aligned");
    assert_eq!(s.region_at(at).unwrap().len, GUEST_PAGE);
    assert!(s.region_at(at + GUEST_PAGE).is_none(), "the tail is a hole");
    assert!(!s.is_trapping(at), "a lenient hole costs nothing");
    let maps: Vec<_> = s.mapped_regions().into_iter().filter(|r| r.start >= at && r.start < at + host_page()).collect();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].len, GUEST_PAGE);
}

#[test]
fn unmapping_every_part_frees_the_host_page() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.unmap(at, GUEST_PAGE).unwrap();
    assert!(s.region_at(at).is_none());
    s.unmap(at + GUEST_PAGE, host_page() - GUEST_PAGE).unwrap();
    assert!(s.regions().iter().any(|r| r.is_free() && r.start <= at && r.end() >= at + host_page()), "free again");
}

#[test]
fn a_fixed_map_over_part_of_a_split_page_keeps_the_neighbours() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    unsafe { (s.host_addr(at) as *mut u8).write(7) };
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    s.map_anonymous(Placement::Fixed(at + GUEST_PAGE), GUEST_PAGE, Protection::Read, CommitPolicy::Lazy).unwrap();
    assert_eq!(unsafe { (s.host_addr(at) as *const u8).read() }, 7, "the neighbour's byte survives");
    assert_eq!(s.region_at(at).unwrap().protection, Protection::ReadWrite);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().protection, Protection::Read);
}

#[test]
fn protect_over_a_hole_is_enomem_and_changes_nothing() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(s.protect(at, 2 * GUEST_PAGE, Protection::Read).is_err());
    assert_eq!(s.region_at(at).unwrap().protection, Protection::ReadWrite, "unchanged");
}

#[test]
fn execute_is_the_guest_view_not_the_hosts() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap();
    assert!(s.any_executable(at, GUEST_PAGE));
    assert!(!s.any_executable(at + GUEST_PAGE, GUEST_PAGE));
}
```

- [ ] **Step 2: Run and confirm they fail.**
  Run: `cargo test -p omni-mem --test subpage`
  Expected: failures (`Misaligned` errors on the 4 KiB calls).
- [ ] **Step 3: Implement the algorithm above.**
  * The alignment checks (`check_aligned`, `round_size`) use `self.guest_page_size()` for these three
    entry points, and still `self.page` for everything else.
  * `region_at` stays cache-first: a remembered entry is only ever the clipped or narrowed one
    (`cache::remember` is given the narrowed `RegionInfo`). Clipping looks up the neighbouring
    tracked pages with `split.range(..=g).next_back()` and `split.range(g..).next()`.
  * `regions(true|false)`: walk the existing entries and expand each tracked host page inside an
    entry into runs, merging adjacent runs with equal `(protection, mapping)`.
- [ ] **Step 4: Run and confirm they pass,** then the full `omni-mem` suite.
  Run: `cargo test -p omni-mem`
  Expected: all green.
- [ ] **Step 5: Commit** `feat(mem): mapping, protecting and unmapping 4 KiB parts of a host page`.

---

### Task 5: Reaching a split page: `access_ptr`, chunks, `write_forced`, `discard`, strict gaps, stats

**Files:**
- Modify: `crates/omni-mem/src/space.rs`, `subpage.rs`, `lib.rs`
- Test: `crates/omni-mem/tests/subpage.rs`

**Interfaces:**
- Produces:

```rust
/// Where an admitted access should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPtr {
    /// The ordinary host address (`host_addr`): no split page is touched.
    Direct(*mut u8),
    /// Every byte is in trapping host pages: their read-write alias.
    Alias(*mut u8),
    /// The range mixes trapping and non-trapping host pages: copy it in chunks.
    Straddle,
}
impl GuestSpace {
    pub fn access_ptr(&self, address: GuestAddr, len: usize) -> AccessPtr;
    /// Call `f(guest_addr, ptr, len)` for each host-page-bounded chunk of the range that differs
    /// in `AccessPtr` kind (one call when there is no trapping page).
    pub fn for_each_access_chunk(&self, address: GuestAddr, len: usize, f: impl FnMut(GuestAddr, *mut u8, usize));
    pub fn set_strict_gaps(&self, address: GuestAddr, len: usize, strict: bool);
    pub fn note_split_served(&self, address: GuestAddr);
    pub fn split_stats(&self) -> SplitStats;
}
#[derive(Debug, Clone, Default)]
pub struct SplitStats { pub tracked: usize, pub trapping: usize, pub served_total: u64, pub top: Vec<(GuestAddr, u64)> }
```

  * `access_ptr` is lock-free when `count == 0`.
  * `note_split_served` bumps an atomic total always. It updates the per-page map only under a
    `try_lock` (never blocks the slow path).
  * `write_forced` on a trapping page writes through the alias, with no flip.
  * `discard` never decommits a tracked page; it zeroes its parts through `with_writable_view`.
  * `OMNI_MEM_REPORT` output (grep for where it is printed) adds a line from `split_stats()` when
    `tracked > 0`.

- [ ] **Step 1: Write the failing tests:**

```rust
use omni_mem::AccessPtr;

#[test]
fn a_trapping_page_is_reached_through_its_alias_and_the_host_sees_the_write() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 8) else { panic!("alias") };
    unsafe { p.write(9) };
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u8).read() }, 9);
    assert!(matches!(s.access_ptr(at + host_page(), 8), AccessPtr::Direct(_) ) || s.region_at(at + host_page()).is_none());
}

#[test]
fn a_range_across_a_trapping_and_an_ordinary_page_is_copied_in_chunks() {
    if !overlay_expected() { return; }
    let s = space(Some(GUEST_PAGE));
    let at = s.map_anonymous(Placement::Anywhere { align: host_page() }, 2 * host_page(), Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert_eq!(s.access_ptr(at + host_page() - 8, 16), AccessPtr::Straddle);
    let mut chunks = Vec::new();
    s.for_each_access_chunk(at + host_page() - 8, 16, |g, _, n| chunks.push((g, n)));
    assert_eq!(chunks, vec![(at + host_page() - 8, 8), (at + host_page(), 8)]);
}

#[test]
fn write_forced_into_a_read_only_part_lands_and_keeps_its_protection() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    s.write_forced(at, &[5]).unwrap();
    assert_eq!(unsafe { (s.host_addr(at) as *const u8).read() }, 5);
    assert_eq!(s.region_at(at).unwrap().protection, Protection::Read);
}

#[test]
fn discard_of_a_split_part_zeroes_it_and_keeps_the_alias_live() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 1) else { panic!() };
    unsafe { p.write(3) };
    s.discard(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert_eq!(unsafe { p.read() }, 0);
    unsafe { p.write(4) };
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u8).read() }, 4, "still one memory");
}

#[test]
fn a_strict_gap_makes_its_host_page_refuse_and_a_lenient_one_does_not() {
    if !overlay_expected() { return; }
    let (s, at) = one_page();
    s.set_strict_gaps(at, host_page(), true);
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(s.is_trapping(at), "read-write parts beside a strict gap trap");
    let (l, at2) = one_page();
    l.unmap(at2 + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(!l.is_trapping(at2));
}

#[test]
fn splitting_a_file_view_page_keeps_its_bytes() {
    if !overlay_expected() { return; }
    let dir = std::env::temp_dir().join(format!("omni-subpage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    let bytes: Vec<u8> = (0..host_page()).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let s = space(Some(GUEST_PAGE));
    let backing = omni_mem::Backing::open_named(&path, omni_mem::MapExecutability::NotExecutable, "f").unwrap();
    let at = s.map_file(&backing, 0, Placement::Anywhere { align: host_page() }, host_page(), Protection::Read).unwrap();
    s.protect(at + GUEST_PAGE, GUEST_PAGE, Protection::ReadWrite).unwrap();
    let got = unsafe { std::slice::from_raw_parts(s.host_addr(at) as *const u8, host_page()) };
    assert_eq!(got, &bytes[..]);
}
```

  (Check the exact `MapExecutability` variant name in `omni-mem/src/backing.rs` and use it.)
- [ ] **Step 2: Run and confirm they fail** (`cargo test -p omni-mem --test subpage`).
- [ ] **Step 3: Implement.**
  * `access_ptr`: walk the host pages of the range, checking `is_trapping` on each. All
    non-trapping gives `Direct(host_addr)`, all trapping gives `Alias(alias_base + (g - base))`,
    and a mix gives `Straddle`.
  * `for_each_access_chunk`: the same walk, emitting maximal runs of one kind.
- [ ] **Step 4: Run and confirm they pass** (`cargo test -p omni-mem`, all green).
- [ ] **Step 5: Commit** `feat(mem): a split page is reached through its alias; chunks, strict gaps, stats`.

---

### Task 6: The pager declines a trapping page at once

**Files:**
- Modify: `crates/omni-mem/src/pager.rs` (`handle_fault` at 339, `PagerStats`)
- Test: `crates/omni-mem/src/pager.rs` `mod tests`

- [ ] **Step 1: Write the failing test** in `pager.rs`'s tests, following the existing
  `inner_over` pattern:

```rust
#[test]
fn a_fault_on_a_trapping_page_is_declined_at_once_and_counted() {
    let space = Arc::new(crate::GuestSpace::with_config(crate::GuestSpaceConfig {
        size: 1 << 30, guest_page: Some(crate::GUEST_PAGE), ..crate::GuestSpaceConfig::default()
    }).unwrap());
    if !space.subpages_active() { return; }
    let page = space.page_size();
    let at = space.map_anonymous(crate::Placement::Anywhere { align: page }, page, Protection::ReadWrite, crate::CommitPolicy::Eager).unwrap();
    space.protect(at, crate::GUEST_PAGE, Protection::Read).unwrap();
    let inner = inner_over(Arc::clone(&space));
    reset_thread_state();
    let fault = Fault { address: space.host_addr(at + crate::GUEST_PAGE), access: FaultAccess::Write, ..Fault::default() };
    for _ in 0..3 {
        assert_eq!(handle_fault(&inner as *const _ as usize, &fault), FaultOutcome::NotOurs);
    }
    let stats = stats_of(&inner);
    assert_eq!(stats.split_declined, 3);
    assert_eq!(stats.retries_exhausted, 0, "no zero-commit streak");
    assert!(stats.is_consistent());
}
```

  (Construct `Fault` exactly as the file's other tests do; adjust the field names to theirs.)
- [ ] **Step 2: Run and confirm it fails** (`cargo test -p omni-mem --lib pager`).
- [ ] **Step 3: Implement.**
  * In `handle_fault`, after translating to a guest address and before `resolve`: if
    `inner.space.is_trapping(address & !(page - 1))`, then `inner.split_declined.fetch_add(1)` and
    return `inner.record(FaultOutcome::NotOurs)`.
  * Add `split_declined: u64` to `PagerStats`, documented as a subset of `declined`.
- [ ] **Step 4: Run and confirm it passes** (`cargo test -p omni-mem`).
- [ ] **Step 5: Commit** `fix(mem): the pager declines a split page's fault at once`.

---

### Task 7: `omni-cpu`: guest code against split pages

**Files:**
- Modify: `crates/omni-cpu/src/dynarmic/callbacks.rs` (`fetch` at 42, `data_ptr` at 78, the read/write
  macros at 197/235, `cb_read128`/`cb_write128`)
- Modify: `crates/omni-cpu/src/dynarmic/mod.rs` (the slice invariant at 2119-2180; a `split_served`
  counter on `CpuCtx`)
- Modify: `crates/omni-cpu/tests/harness/mod.rs` (`Guest::with_space_config`)
- Test: `crates/omni-cpu/tests/subpage.rs`

**Interfaces:**
- Consumes: `GuestSpace::access_ptr`, `AccessPtr`, `note_split_served` (Task 5).
- Produces:
  - `CpuCtx::split_served: u64`
  - `DynarmicCpu::split_served(&self) -> u64`
  - Harness: `Guest::with_space_config(options: DynarmicOptions, guest_page: Option<usize>) -> Guest`

- [ ] **Step 1: Write the failing tests** (`tests/subpage.rs`):

```rust
//! Guest code on a 4 KiB-split host page: each part's protection holds (spec 2026-09-29).
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]
mod harness;
use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection, GUEST_PAGE};

fn linux_options() -> DynarmicOptions {
    DynarmicOptions { recompile_on_declined_fault: false, ..DynarmicOptions::default() }
}

/// A guest with 4 KiB pages, and one committed read-write host page in its space.
fn guest_and_page() -> (Guest, usize) {
    let g = Guest::with_space_config(linux_options(), Some(GUEST_PAGE));
    let page = g.space.page_size();
    let at = g.space.map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    (g, at)
}

/// `str x1, [x0]; ret`
fn store(g: &Guest, at: usize, value: u64) -> ExitReason {
    let entry = g.load(&[str_imm(1, 0, 0), ret(30)]);
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(0), at as u64);
    cpu.set_x(x(1), value);
    cpu.set_x(x(30), sentinel as u64);
    cpu.run(entry, RunLimit::Unlimited).expect("no degraded slice")
}

/// `ldr x2, [x0]; ret`
fn load(g: &Guest, at: usize) -> (ExitReason, u64) {
    let entry = g.load(&[ldr_imm(2, 0, 0), ret(30)]);
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(0), at as u64);
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("no degraded slice");
    (exit, cpu.x(x(2)))
}

#[test]
fn two_4_kib_pages_in_one_host_page_keep_their_own_protection() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert!(matches!(store(&g, at + GUEST_PAGE, 0xAB), ExitReason::Returned { .. }), "the read-write page takes the store");
    assert!(matches!(store(&g, at, 0xCD), ExitReason::MemoryFault { address, .. } if address == at), "the read-only page refuses it");
    let (exit, v) = load(&g, at + GUEST_PAGE);
    assert!(matches!(exit, ExitReason::Returned { .. }));
    assert_eq!(v, 0xAB);
}

#[test]
fn a_prot_none_4_kib_page_traps_and_its_neighbours_do_not() {
    let (g, at) = guest_and_page();
    let page = g.space.page_size();
    let none = at + GUEST_PAGE.min(page - GUEST_PAGE);
    g.space.protect(none, GUEST_PAGE, Protection::None).unwrap();
    assert!(matches!(load(&g, none).0, ExitReason::MemoryFault { address, .. } if address == none));
    for n in [at, at + page - 8] {
        if n >= none && n < none + GUEST_PAGE { continue; }
        assert!(matches!(load(&g, n).0, ExitReason::Returned { .. }), "{n:#x}");
        assert!(matches!(store(&g, n, 1), ExitReason::Returned { .. }), "{n:#x}");
    }
    // A 16-byte access straddling into the PROT_NONE part faults.
    let entry = g.load(&[ldp(2, 3, 0, 0), ret(30)]);
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(0), (none - 8) as u64);
    cpu.set_x(x(30), sentinel as u64);
    assert!(matches!(cpu.run(entry, RunLimit::Unlimited).unwrap(), ExitReason::MemoryFault { .. }));
}

#[test]
fn a_served_access_is_not_a_degraded_slice() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    // Many stores to the read-write part in one slice: each is served, none degrades the slice.
    let entry = g.load(&[str_imm(1, 0, 0), sub_imm(3, 3, 1), cbnz(3, -8), ret(30)]);
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(0), (at + GUEST_PAGE) as u64);
    cpu.set_x(x(1), 7);
    cpu.set_x(x(3), 1000);
    cpu.set_x(x(30), sentinel as u64);
    assert!(matches!(cpu.run(entry, RunLimit::Unlimited).expect("not degraded"), ExitReason::Returned { .. }));
    if g.space.subpages_active() {
        assert!(cpu.split_served() >= 1);
    } else {
        assert_eq!(cpu.split_served(), 0, "a 4 KiB host never serves");
    }
}

#[test]
fn code_in_an_executable_part_beside_a_prot_none_part_runs() {
    let (g, at) = guest_and_page();
    let page = g.space.page_size();
    // Code in part 0, PROT_NONE in the last part.
    let code = [movz(0, 42), ret(30)];
    for (i, w) in code.iter().enumerate() {
        g.space.write_forced(at + 4 * i, &w.to_le_bytes()).unwrap();
    }
    g.space.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap();
    g.space.protect(at + page - GUEST_PAGE, GUEST_PAGE, Protection::None).unwrap();
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(30), sentinel as u64);
    assert!(matches!(cpu.run(at, RunLimit::Unlimited).unwrap(), ExitReason::Returned { .. }));
    assert_eq!(cpu.x(x(0)), 42);
}

#[test]
fn four_threads_increment_a_split_page_counter() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let counter = at + GUEST_PAGE;
    // loop: ldaxr x2,[x0]; add x2,x2,#1; stlxr w4,x2,[x0]; cbnz w4,loop; subs x3,x3,#1; b.ne loop; ret
    let entry = g.load(&[ldaxr(2, 0), add_imm(2, 2, 1), stlxr(4, 2, 0), cbnz(4, -12), subs_imm(3, 3, 1), b_ne(-20), ret(30)]);
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                let (mut cpu, sentinel) = g.thread();
                cpu.set_x(x(0), counter as u64);
                cpu.set_x(x(3), 500);
                cpu.set_x(x(30), sentinel as u64);
                assert!(matches!(cpu.run(entry, RunLimit::Unlimited).unwrap(), ExitReason::Returned { .. }));
            });
        }
    });
    assert_eq!(g.read_u64(counter), 2000, "no lost update");
}
```

  Use the encoders `harness/a64.rs` has. **Before writing, list its functions** and add any
  missing ones (`ldp`, `movz`, `sub_imm`, `subs_imm`, `cbnz`, `b_ne`, `ldaxr`, `stlxr`,
  `add_imm`) there, each with its encoding cited from the ARM ARM as the existing ones are.
  The `Guest` helpers (`read_u64`, `thread` from several threads) must exist; adjust the test to
  the harness's real signatures.
- [ ] **Step 2: Run and confirm they fail.**
  Run: `cargo test -p omni-cpu --features dynarmic --test subpage`
  Expected: `with_space_config` missing. Once the harness helper exists, the first test should
  fail with a host crash or `DegradedMemoryPath`; record which.
- [ ] **Step 3: Implement.**
  * The harness gets a `with_space_config` twin of `high_guest_space()` that passes `guest_page`.
  * `data_ptr` returns an enum of its own, `DataPtr { Direct(*mut u8), Alias(*mut u8), Straddle }`,
    from `access_ptr` after the existing `resolve`. `Alias` increments `self.split_served` and
    calls `space.note_split_served`.
  * The read/write macros handle `Straddle` with `for_each_access_chunk`, assembling or splitting
    the little-endian value.
  * The exclusive and compare-and-swap callbacks refuse `Straddle` as a fault (it cannot happen
    for an aligned access).
  * `fetch` uses `access_ptr(address, 4)`: `Direct` or `Alias`; `Straddle` is impossible for an
    aligned 4-byte read.
  * Slice invariant: capture `split_served` before and after the slice. The degraded condition
    becomes `delta_slow - delta_served != 0`, with a comment explaining why a served split-page
    access is not a degraded block.
- [ ] **Step 4: Run it, then the whole `omni-cpu` suite.**
  Run: `cargo test -p omni-cpu --features dynarmic`
  Expected: all green, including `declined_fault.rs` and `low_window.rs`.
- [ ] **Step 5: Commit** `feat(cpu): guest code on a split page -- served through the alias, never a degraded slice`.

---

### Task 8: `omni-linux`: the guest is told 4 KiB, and gets it

**Files:**
- Modify: `crates/omni-linux/src/process.rs:203` (`reserve_space`: `guest_page: Some(omni_mem::GUEST_PAGE)`)
- Modify: `crates/omni-linux/src/mm.rs`:
  * `Mm::new` sets `page = space.guest_page_size()`;
  * unhinted placement uses `align: space.page_size()` (the host page);
  * the file path checks congruence;
  * shared refusal;
  * `kernel_write` becomes `write_forced`;
  * delete `SMALL_PAGE`, `union`, `subpages`, `forget_subpages` and `protect_widened`, and the
    branch in `protect` that calls it.
- Modify: `crates/omni-linux/src/guest.rs`: `check` returns `(start, len)` validated; `read`,
  `write_holding_layout` and `read_holding_layout` copy through `for_each_access_chunk`;
  `atomic_u32` uses `access_ptr` (`Direct` or `Alias`).
- Modify: `crates/omni-linux/src/shm.rs:65`: `vm::page_size()` becomes the process's `mm.page_size()`
  (read the call site; pass the page in).
- Delete: `crates/omni-linux/src/pagecompat.rs`. Remove `mod pagecompat` from `lib.rs` and the call
  at `fd.rs:252`.
- Test: `crates/omni-linux/tests/mm.rs` (update); create `crates/omni-linux/tests/page_size.rs`.

**Interfaces:**
- Consumes: Tasks 3-5's `GuestSpace` API.
- Produces: `Mm::page_size() == 4096` on every host that has the overlay or a 4 KiB page. The
  direct path is unaffected.

- [ ] **Step 1: Write the failing tests.** In `tests/mm.rs`:
  * Replace the `if pg > 4096 { ... EINVAL }` block in
    `the_page_is_the_guest_spaces_page_and_mappings_are_exact_at_it` with
    `assert_eq!(pg, 4096, "the guest's page is 4 KiB on every host that can give it");`
    guarded by `if omni_platform::vm::supports_alias() || omni_platform::vm::page_size() == 4096`.
  * Delete the tests that pinned `protect_widened` (grep `widened` in `tests/mm.rs`).
  * Add:

```rust
#[test]
fn a_4_kib_mprotect_inside_a_host_page_is_exact_for_the_kernel_too() {
    let (p, mut t, _, pg) = process();
    let at = mmap(&p, &mut t, [0, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at + 4096, 4096, PROT_READ, 0, 0, 0]), 0);
    assert!(p.mem.write(at + 4096, b"x").is_err(), "EFAULT for the read-only 4 KiB");
    assert!(p.mem.write(at, b"x").is_ok() && p.mem.write(at + 2 * 4096, b"x").is_ok());
    assert_eq!(pg, 4096);
}

#[test]
fn a_read_into_a_buffer_across_a_split_page_lands_whole() {
    let (p, mut t, _, _) = process();
    let host = p.mem.space().page_size() as u64;
    let at = mmap(&p, &mut t, [0, 2 * host, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at, 4096, PROT_READ, 0, 0, 0]), 0);
    let bytes: Vec<u8> = (0..(2 * host - 4096) as usize).map(|i| i as u8).collect();
    p.mem.write(at + 4096, &bytes).expect("a kernel copy across a split and an ordinary page");
    assert_eq!(p.mem.read(at + 4096, bytes.len()).unwrap(), bytes);
}

#[test]
fn mremap_moves_split_edges_intact() {
    let (p, mut t, _, _) = process();
    let at = mmap(&p, &mut t, [0, 6 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(at, &[1; 6 * 4096]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at + 4096, 4096, PROT_READ, 0, 0, 0]), 0);
    let to = p.syscall(&mut t, nr::MREMAP, [at, 6 * 4096, 6 * 4096, 1, 0, 0]) as i64;
    assert!(to > 0);
    let to = to as u64;
    assert_eq!(p.mem.read(to, 6 * 4096).unwrap(), vec![1; 6 * 4096]);
    // mremap keeps the first page's protection for the whole range (sys_mremap's rule); what
    // matters here is that the move of a split range neither fails nor loses bytes.
}

#[test]
fn a_4_kib_file_mapping_at_a_4_kib_offset_reads_the_file() {
    let (p, mut t, s, _) = process();
    // /system/lib64/libx.so is 6000 bytes of (i % 251) + 1.
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 4096, PROT_READ, MAP_PRIVATE, fd, 4096]) as u64;
    let got = p.mem.read(at, 6000 - 4096).unwrap();
    let want: Vec<u8> = (4096..6000u32).map(|i| (i % 251) as u8 + 1).collect();
    assert_eq!(got, want);
}
```

  In `tests/page_size.rs` (follow `tests/a1_toybox.rs`'s boot helper exactly for running a sysroot
  binary and capturing stdout):

```rust
//! What bionic tells a program its page is: 4 KiB on every host (spec 2026-09-29).
mod common;

#[test]
fn getconf_pagesize_is_4096() {
    let out = common::run_sysroot(&["/system/bin/toybox", "getconf", "PAGESIZE"]);
    assert_eq!(out.stdout.trim(), "4096", "{out:?}");
}

#[test]
fn at_pagesz_is_4096() {
    // od prints /proc/self/auxv as (key, value) u64 pairs; AT_PAGESZ is key 6.
    let out = common::run_sysroot(&["/system/bin/toybox", "od", "-An", "-tu8", "-v", "/proc/self/auxv"]);
    let words: Vec<u64> = out.stdout.split_whitespace().map(|w| w.parse().unwrap()).collect();
    let pagesz = words.chunks(2).find(|kv| kv[0] == 6).map(|kv| kv[1]);
    assert_eq!(pagesz, Some(4096), "{out:?}");
}
```

  If `common` has no `run_sysroot`, write it in `tests/common/mod.rs` by extracting what
  `a1_toybox.rs` does, and make `a1_toybox.rs` use it (no duplication).
- [ ] **Step 2: Run and confirm they fail on the M1.**
  Run: `cargo test -p omni-linux --test mm --test page_size`
  Expected: `pg == 16384`; `getconf` prints `16384`.
- [ ] **Step 3: Implement** the file list above. Two details:
  * **File congruence** in `Mm::map`: before trying a view, require
    `req.offset % host == 0`, and for `Fixed` also `req.addr % host == 0` (host is
    `self.space.page_size()`). Otherwise go straight to the private-copy branch.
  * **Shared refusal:** in the `shm` and `shared` branches, if `req.offset % host != 0` or (`fixed`
    and `req.addr % host != 0`), call `p.refusals.record("mmap: MAP_SHARED at a 4 KiB offset on a larger host page".into(), t.pc, t.lr)`
    and return `Err(EINVAL)`.
- [ ] **Step 4: Run and confirm they pass,** then the fast `omni-linux` suite.
  Run: `cargo test -p omni-linux` (the ignored gates stay ignored).
  Expected: all green; report any red test by name.
- [ ] **Step 5: Commit** in two commits:
  * `feat(linux): the guest's page is 4 KiB on every host (AT_PAGESZ, mmap, mprotect, munmap)`
  * `refactor(linux): pagecompat and widened mprotect go -- a 4 KiB guest needs neither`

---

### Task 9: The strict-gap escape hatch, and scudo checked

**Files:**
- Modify: `crates/omni-linux/src/mm.rs` (`label`: after recording the name, match `OMNI_STRICT_GAPS`)
- Test: `crates/omni-linux/tests/mm.rs`

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn a_named_range_on_the_strict_list_makes_its_gaps_fault() {
    std::env::set_var("OMNI_STRICT_GAPS", "strict-test:");
    let (p, mut t, s, _) = process();
    let host = p.mem.space().page_size() as u64;
    let at = mmap(&p, &mut t, [0, host, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(s, b"strict-test:x\0").unwrap();
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [0x5356_4d41, 0, at, host, s, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + 4096, 4096, 0, 0, 0, 0]), 0);
    if p.mem.space().subpages_active() {
        assert!(p.mem.space().is_trapping(at), "a strict gap makes its read-write neighbours trap");
    }
}
```

- [ ] **Step 2: Run and confirm it fails** (`cargo test -p omni-linux --test mm a_named_range`).
- [ ] **Step 3: Implement.** Parse `OMNI_STRICT_GAPS` once (`OnceLock<Vec<Vec<u8>>>`, split on
  `,`). In `Mm::label` (and in `map` for file paths), if the name (without `[anon:` and `]`) starts
  with a listed prefix, call `self.space.set_strict_gaps(start, len, true)`.
- [ ] **Step 4: Run and confirm it passes.**
- [ ] **Step 5: Check scudo against the real allocator** (evidence, not code). In the Task 12
  run's log (`OMNI_TRACE_APP` on), use a script to count the app process's `mmap(PROT_NONE)` →
  `mmap(MAP_FIXED, RW)` sub-range patterns and any `munmap` of a 4 KiB piece inside a range named
  `scudo:*`. The result goes in the Task 12 notes:
  * no `munmap`s of 4 KiB guards: choice A covers scudo, and the default stays empty;
  * otherwise: document `OMNI_STRICT_GAPS=scudo:` as the recommended setting.
- [ ] **Step 6: Commit** `feat(linux): OMNI_STRICT_GAPS -- named ranges whose gaps must fault`.

---

### Task 10: The 4 KiB library itself

**Files:**
- Modify: `crates/omni-linux/tests/c2_apk_in_app_process.rs` (remove the exclusion of
  `libzstd-jni-1.5.7-6.so` that `9f5632b` added; add a layout assertion)

- [ ] **Step 1: Write the failing test.**
  * Put `libzstd-jni-1.5.7-6.so` back in c2's library list, as a `System.load` or `dlopen` only.
    Its `JNI_OnLoad` still needs a running app, so load it with `RTLD_NOW` through the test's
    existing `dlopen` path if it has one. Otherwise assert only on the maps.
  * After loading, read `/proc/<pid>/maps` (the test's existing helper). Assert that the library's
    `PT_LOAD` segments sit at `bias + p_vaddr` with `offset == p_offset & !0xfff`, using the program
    headers parsed from the APK entry with `omni-elf`.
  * If Task 0 found a checkable mechanism, assert it here too.
- [ ] **Step 2: Run on the M1 before Task 8 is merged** (or on its parent commit) and confirm it fails.
  Run: `cargo test --release -p omni-linux --test c2_apk_in_app_process -- --ignored --nocapture`
  Expected: offsets disagree (pagecompat moved the segments).
- [ ] **Step 3: Run after Task 8 and confirm it passes.**
- [ ] **Step 4: Commit** `test(linux): Roblox's 4 KiB libzstd-jni loads where its headers say`.

---

### Task 11: Full suites, and the 4 KiB-host regression under Rosetta

- [ ] **Step 1: The M1 suites.**
  Run: `cargo test --release -p omni-platform -p omni-mem -p omni-cpu -p omni-linux 2>&1 | tail -40`
  (plus `--features dynarmic` where the crate needs it). Name every red test.
- [ ] **Step 2: The 4 KiB host.** Check whether this Mac can build `x86_64-apple-darwin` (the
  `target-x86` directory suggests a prior build). If it can, run
  `cargo test --target x86_64-apple-darwin -p omni-mem -p omni-cpu --features dynarmic --test subpage`.
  Rosetta's page is 4 KiB, so every `subpage` test takes its 4 KiB branch, and `subpages_active()`
  is false. If the cross build is not possible, say so and name the Windows/Linux runs as the
  owner's to make.
- [ ] **Step 3: Windows build check.** Run `cargo check --target x86_64-pc-windows-msvc -p omni-mem -p omni-linux`
  if the target is installed. Otherwise note it.
- [ ] **Step 4: Commit** any test-only fixes found, one per fix, with the failing test first.

---

### Task 12: The gate (M1 acceptance)

- [ ] **Step 1: Memory.** Record `memory_pressure | tail -1` and `vm_stat`. macOS has no commit
  charge; require at least 50% free. Close nothing of the owner's.
- [ ] **Step 2: Run the gate.**

```sh
OMNI_TRACE_APP= tools/aosp_play.sh --apk ~/Desktop/Roblox-2.740.931.apk \
  --cookie ~/Desktop/cookies/HezMi_ImYu916.txt --place 8737899170 --minutes 10 > "$SCRATCH/gate.log" 2>&1
```

- [ ] **Step 3: Verify from the logs, by script.**
  * `libzstd-jni` mapped, and no fatal signal in `com.roblox.client`.
  * The engine's join-to-in-world markers (the same ones `r_roblox.rs` prints).
  * The fps line after 1 minute and after 5.
  * `split_stats` lines: tracked, trapping, served per second, top pages.
- [ ] **Step 4: Screenshot** in-world after the one-minute mark, from the run's `-shots` directory
  (`OMNI_R_SHOT_SECS`), or `screencapture -x` of the window.
- [ ] **Step 5: Send** the screenshot plus a one-line verdict with `SendUserFile`.
- [ ] **Step 6: Docs.**
  * Add D42 to `docs/DECISIONS.md`: 4 KiB guest pages on any host, the overlay, choice A, the
    escape hatch.
  * Update `docs/ports/macos.md`: "The page is the host's" becomes the 4 KiB section, the Open item
    is closed with the gate's numbers, and the Linux 16 KiB follow-up is noted.
  * Update the memory file `mac-port-state.md`.

  Commit `docs(macos): D42 -- 4 KiB guest pages; Roblox 2.740.931 in PS99 on the M1`.

---

### Task 13 (M2, conditional): performance from the numbers

Take this task only if Task 12's fps is clearly below the Windows/Linux same-APK figure the owner
supplies (32-36 fps), or served traps exceed about 20k/s.

- [ ] **Step 1:** From `split_stats` top pages and their `/proc` names, classify the hot split pages:
  guard beside data, a 4 KiB ELF boundary, or other.
- [ ] **Step 2:** For each class, write the failing perf test first (served count after a fixed
  workload in `omni-cpu/tests/subpage.rs`), then the fix:
  * **Guard beside data (scudo or pthread):** guard-aware placement in `Mm::map`. An unhinted
    `PROT_NONE` reservation is placed so that its first 4 KiB ends a host page, and the preceding
    parts of that host page are left as `Hole`s.
  * **Hot instruction:** a dynarmic patch (`0032`, arm64 only) passing the fault address to
    `FastmemCallback`, and a config callback that asks whether to recompile *this* fault. Omni
    answers yes only for a trapping page. Carry it per `patches/README.md`.
- [ ] **Step 3:** Re-run the gate, and record before and after in `docs/ports/macos.md`.
