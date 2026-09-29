# 4 KiB guest pages on any host page (sub-host-page protection)

Date: 2026-09-29. Branch: `mac-port`. Status: design approved in conversation; this spec awaits review.

## Problem

`omni-linux` tells the guest the host's page size (`Mm::page` = `GuestSpace::page_size()` =
`vm::page_size()`, sent as `AT_PAGESZ`), because a guest address is a host address (D4) and the host
changes protections only in whole host pages. On the M1 the host page is 16 KiB, so the guest is a
16 KiB-page Android 15. Windows and x86-64 Linux have 4 KiB pages, so their guest is an ordinary
4 KiB Android.

Roblox 2.740.931 (the Delta build) needs
`libzstd-jni-1.5.7-6.so` at startup. That library is packed and linked only for 4 KiB pages
(`p_align` 0x1000). On the Mac, `pagecompat` currently re-lays the library out, and `protect_widened`
approximates its 4 KiB `mprotect`s with unions. With both, the library loads and decrypts. About a
minute later it reads through a function table it never set up (`SIGSEGV` at `0x40`) and the app
dies. Shimming one library does not generalise. The fix belongs in the platform: the guest should
always have 4 KiB pages.

## Goal and acceptance

* Every process on the real-AOSP path (`omni-linux`) gets 4 KiB pages on every host:
  `AT_PAGESZ`, `getpagesize()` and `sysconf(_SC_PAGESIZE)` all say 4096, and every `mmap`,
  `mprotect`, `munmap` and `madvise` is exact to 4 KiB.
* On a host whose page is 4 KiB (Windows, x86-64 Linux) this changes nothing. No new code runs on
  the access path; the Windows and Linux builds and their performance are unchanged.
* Nothing hard-codes 16384: the host page size is read at run time, and the machinery turns on only
  when the host page is larger than 4 KiB.
* **The gate:** `tools/aosp_play.sh --apk ~/Desktop/Roblox-2.740.931.apk --cookie
  ~/Desktop/cookies/<file> --place 8737899170` on the M1 logs in and joins PS99, then keeps
  rendering past the one-minute mark where it now dies, at fps comparable to a same-day
  Windows/Linux run of the same APK. The logs must show that `libzstd-jni` loaded and did not
  crash. The evidence is a screenshot taken in-world after the one-minute mark.

## Out of scope

* The direct path (`omnidroid play`, `omni-android`'s own loader). It keeps host-page spaces and
  its `AlignBelowPageSize` refusal. The new behaviour is opt-in per space, and only `omni-linux`
  opts in.
* The HVF native backend (D34), which is not on the AOSP path.
* Linux hosts with 16 KiB or 64 KiB pages (for example Asahi). They have no alias primitive yet
  (see below), so they keep guest page = host page, exactly as today. This is a documented
  follow-up.

## Key facts from the code (why the design is shaped this way)

1. **Guest execute permission is already enforced in software.** dynarmic reads guest code through
   `cb_read_code`, which calls `CpuCtx::fetch` and then `resolve(ReadExecute)` against the region
   map. The host execute bit on guest pages is never relied on, so a host page's derived
   protection only has to get read and write right.
2. **The trap path already exists and runs in the right order.** On macOS, `omni-platform`'s Mach
   thread-port handler sees `EXC_BAD_ACCESS` before dynarmic does. It runs `omni-mem`'s pager on
   the faulting thread. If the pager declines, dynarmic's task-port handler sends the access to the
   slow-path callback. The Linux personality sets `recompile_on_declined_fault: false`, so this
   happens once and the block keeps fastmem.
3. **Every host-side guest access goes through one of three choke points:**
   * `omni-cpu`'s `CpuCtx::fetch` and `CpuCtx::data_ptr`;
   * `omni-linux`'s `GuestMem::check`, used by every syscall copy, futex word and signal frame;
   * `GuestSpace::write_forced`.

   All of them ask `region_at` or `access::admit`.
4. **A host page that allows an access never traps.** So the host protection of a page with mixed
   4 KiB protections must be the **least** permissive of them. The software table then serves the
   accesses it allows and refuses the rest. (The brief's "most permissive" would enforce nothing.)
5. **A served access must reach memory whose host page forbids it.** Changing the page's protection
   for the moment of the access would race other guest threads and cost two `mprotect` calls and a
   TLB shootdown each time. Instead, a split host page gets a read-write **alias**: the same memory,
   mapped a second time.

## Design

### Terms

* **Guest page:** 4096 bytes (`omni_mem::GUEST_PAGE`; `SMALL_PAGE` is renamed to this).
* **Host page:** `vm::page_size()`.
* **Part:** one 4 KiB piece of a host page.
* **Uniform host page:** every mapped part has the same protection. The region map is the truth, as
  today.
* **Split host page:** its parts differ. The overlay (below) is the truth for it.

### Unit 1: `omni-platform::vm` -- the alias primitive

* `vm::supports_alias() -> bool`: `true` on macOS, `false` elsewhere.
* `vm::reserve_alias(len) -> Reservation`: `PROT_NONE` address space, with no commit.
* `unsafe vm::alias(src, dst, len)`: makes `[dst, dst + len)` the same memory as `[src, src + len)`,
  read-write, replacing whatever was at `dst`.
  * macOS implementation: `mach_vm_remap(copy = FALSE, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE)`,
    then `mach_vm_protect(RW)`.
  * The source must be committed private anonymous memory; Unit 2 guarantees that.
* `unsafe vm::unalias(dst, len)`: back to a `PROT_NONE` reservation.
* **Probe test (first task):**
  * A write through the alias is read through the source, and the reverse.
  * It still holds after the source is `mprotect`ed to `Read` and to `None`.
  * `phys_footprint` does not double.
  * A fresh `MAP_FIXED` over the source (omni-mem's decommit on macOS) detaches the alias. The test
    pins this so Unit 2 re-aliases after every decommit or replace.

### Unit 2: `omni-mem` -- the sub-page overlay in `GuestSpace`

**Opt-in.** `GuestSpaceConfig::guest_page: Option<usize>`. The overlay is active only when it is
`Some(4096)`, the host page is larger, and `vm::supports_alias()`. Otherwise
`GuestSpace::guest_page_size()` equals `page_size()` and every method behaves byte for byte as
today; there is no overlay object at all. `omni-linux`'s `reserve_space` sets it; `omni-android`
does not.

**State (active spaces only).**
* `SubPages { alias_base, split: BTreeMap<HostPage, [Part; N]>, bitmap }`, where
  `N = host page / 4096`.
* `Part` is one of:
  * `Mapped(Protection, Backing)`, where `Backing` is `Anonymous` or `File { name, offset }`, used
    only for `/proc` naming (see below);
  * `Hole` (lenient);
  * `StrictHole`.
* `bitmap` has one bit per host page of the space and is allocated at the first split. With no split
  page, a check costs one relaxed atomic load of "number of split pages", which is zero.
* The alias reservation is the space's length and is made lazily at the first split.
  `alias(g) = alias_base + (g - base)`, which is independent of the low window.

**Invariants.**
1. A split host page is mapped in the region map as **committed private anonymous memory**. Its
   host protection is the intersection of the read/write bits of its `Mapped` parts and its
   `StrictHole` parts, which count as `None`. `Hole` parts do not count (choice A, below). The
   execute bit is dropped on the host.
2. A split host page has a live alias exactly while it is split.
3. When a host page becomes uniform again (every part has the same `Mapped` protection), it leaves
   the overlay, loses its alias and gets that protection on the host. When every part is a hole,
   the host page is unmapped for real.
4. Any overlay change bumps `GuestSpace::generation`, so every thread's `remembered` cache refetches.

**Splitting a file-view host page privatises it first.** Its 16 KiB are copied, the page is replaced
by anonymous memory, and the bytes are written back. Only then is it aliased. Views are otherwise
kept, and a split page is small. This avoids depending on XNU's copy-on-write behaviour under
`vm_map_remap`, and makes invariant 1 hold.

**Guest-page API.** On an active space, `map_anonymous`, `map_file`, `protect`, `unmap`, `discard`
and `write_forced` accept 4 KiB-aligned addresses and lengths. A request that covers whole host
pages takes today's host path unchanged (the fast path). Only the partly covered host pages at
either edge go through the overlay:
* **map (anonymous):** mark the parts `Mapped` and zero them. If the host page was free, map the
  whole page, with the other parts as `Hole`.
* **map (file) at a 4 KiB offset:** `map_file` needs `addr` and `offset` congruent modulo the host
  page. When they are not, `omni-linux` already falls back to a private copy (anonymous memory filled
  by `pread`), and that copy is a guest-page-exact anonymous mapping.
* **protect:** set the parts.
* **unmap:** set the parts to `Hole`, or `StrictHole` if the range is strict (below).
* After every change, recompute the host protection and apply invariants 1-3.

**Guest view.**
* `region_at(g)` for `g` in a split page returns a `RegionInfo` narrowed to the run of parts in the
  same state:
  * a `Mapped` run has its guest protection and is `committed`;
  * a hole is free.
* `regions()` and `mapped_regions()` return the guest view: split pages are expanded into runs, and
  runs join the neighbouring entries they continue. `/proc/<pid>/maps`, `fork` and `Mm::unmap` see
  4 KiB-exact mappings.
* `host_regions()` keeps the host view for this crate's own use.

**Access.** `admit` is unchanged in its rules and gets its answers from the guest view. The new
`GuestSpace::access_ptr(g, len)` returns the address an admitted access should use:
* `host_addr(g)` unless `[g, g + len)` touches a split page;
* the alias otherwise, if every byte is in split pages;
* `Err(Straddle)` for a range that mixes split and non-split host pages. Callers chunk the range at
  host-page boundaries (`GuestSpace::for_each_access_chunk`).

The alias is always read-write; the permission check has already been made against the guest view.

**Pager.** For a fault address in a split page, `handle_fault` returns `NotOurs` at once, without
committing, retrying or counting a zero-commit streak. Retrying cannot help, because the host page
stays at its intersection. This matters: without it, the zero-commit retry path would spin up to
its bound on every such fault. The pager counts `split_declined` in `PagerStats`, and the counter is
kept consistent with the stats invariant.

**Statistics.** Each `SubPages` counts served accesses per split page and in total. It reports the
top host pages by count through `OMNI_MEM_REPORT`, plus one log line per 10 s while any are served
(address, `/proc` name, parts, count). Milestone 2 is decided from these numbers.

### Unit 3: `omni-cpu` -- the slow path uses the alias

* `CpuCtx::fetch` and `data_ptr` return `access_ptr(g, len)` instead of `host_addr(g)`.
* A data access (at most 16 bytes) that straddles a split and a non-split page is served byte-wise
  through two chunks. An exclusive or compare-and-swap never straddles, because it is aligned.
* The `executable_cache` extent is the narrowed `region_at` result, so it never spans a part with
  another protection.

### Unit 4: `omni-linux` -- the guest is told 4 KiB and gets it

* `reserve_space` asks for `guest_page: Some(4096)`.
* `Mm::page = space.guest_page_size()` is 4096 on the Mac, and `AT_PAGESZ` follows it (it already
  reads `p.mm.page_size()`).
* The page-size consumers named in the survey (`exec.rs:65`, `process.rs:702/1253`, `binder.rs:1067`,
  `shm.rs:65`, `mm.rs:470`, `sys_madvise`, `sys_msync`, `sys_mlock`, `sys_mremap`) use `Mm::page`
  or the space's guest page, never `vm::page_size()`.
* **Placement.** An unhinted mapping is placed host-page-aligned, with its reservation rounded up to
  whole host pages. Its tail becomes `Hole` parts, and different unhinted mappings never share a
  host page. `MAP_FIXED` mappings and hints are exact to 4 KiB.
* **Shared mappings at a 4 KiB offset** that is not congruent with the address modulo the host page
  (memfd, ashmem or file `MAP_SHARED` writable) are refused with `EINVAL` and recorded in
  `p.refusals`. A private copy would silently break sharing. None is expected: their offsets are 0.
* **Removed:**
  * `pagecompat.rs` and its call in `fd.rs:252`, since a real 4 KiB `linker64` maps 4 KiB-aligned
    libraries itself;
  * `protect_widened`, `Mm::subpages` and `union`, which the overlay subsumes.

  Their tests become the 4 KiB tests below.
* **`kernel_write`** becomes `space.write_forced`, which writes a split page through its alias with no
  flip.
* **`GuestMem::check`** returns chunks and a pointer through `access_ptr`. `read`, `write` and
  `read_holding_layout` copy per chunk. `atomic_u32` is aligned, so it is one chunk.

### The strict-gap escape hatch

Choice A is the default: a hole in a host page is lenient, so an access to it may succeed instead of
faulting. Explicit `PROT_NONE` is always enforced.

For an allocator that relies on a hole faulting:
* `GuestSpace::set_strict_gaps(range, bool)` marks a range. Its unmapped parts become `StrictHole`
  (count as `None` in the host protection), and they remain so until the range is mapped again.
* `omni-linux` turns this on from `OMNI_STRICT_GAPS=<name prefix>,...`, matched against the
  `PR_SET_VMA_ANON_NAME` name that `sys.rs` already records through `Mm::label`. It can also be
  turned on for a `/proc` path prefix. The default is empty.

**Scudo:** Android 15's allocator reserves with `mmap(PROT_NONE)` and commits with
`mmap(MAP_FIXED, PROT_READ | PROT_WRITE)` over parts of the reservation. Its guard pages are the
parts it never commits: mapped, `PROT_NONE` and enforced under A. The plan verifies this with a
trace in an app process (scudo-named ranges and their sub-page `PROT_NONE` parts) before relying on
it. If scudo is found to `munmap` a guard, `OMNI_STRICT_GAPS=scudo:` restores strict behaviour.

## Error handling

* An overlay operation that cannot alias, privatise or commit fails the guest syscall with `ENOMEM`,
  leaving the overlay and the region map as they were. The operation is transactional per host page:
  compute, apply to the host, then publish.
* `access_ptr` in the slow path never dereferences memory the guest view refuses; a refusal is the
  existing typed fault, which becomes a guest `SIGSEGV` with `si_addr` the guest address.
* Every fault a guest page means (a denied part, a strict hole) is delivered exactly as today's
  whole-page faults are: once, as the guest's signal.

## Debugging before building: `libzstd-jni`, time-boxed

Before Unit 2, under the current build (16 KiB guest, `pagecompat`), trace:
* the library's `mmap`, `mprotect`, `munmap`, `openat` and `read` of `/proc/self/maps`, and
  `dl_iterate_phdr`;
* the faulting PC, its disassembly and the table address.

The aim is to name what the library computed from 4 KiB assumptions (a page-rounded base, a
re-laid-out segment offset, a union protection it tested). The finding goes into the plan. If it
yields a check that can run without the whole app (for example, "the library's segments are where
its program headers say, relative to its load bias"), that becomes the regression test beside the
gate. The time box is about two hours; the gate remains the final proof either way.

## Testing (TDD: written first, red on the M1)

Every item is in `omni-mem`, `omni-cpu` or `omni-linux` tests, and runs on every host. On a 4 KiB
host each item runs the fast path and asserts that the overlay was never created.

1. **Two parts, two protections.** Map two adjacent 4 KiB pages inside one host page, the first
   `Read` and the second `ReadWrite`.
   * Guest code (dynarmic) storing to the second succeeds.
   * Storing to the first exits `Fault { address = first, Write }`.
   * `GuestMem::write` to each returns `Ok` and `EFAULT` respectively.
2. **One `PROT_NONE` part.** Map a whole host page `ReadWrite`, then `mprotect` one 4 KiB part of it
   `PROT_NONE`.
   * Guest loads from that part fault.
   * Loads and stores to the other parts succeed.
   * A 16-byte access straddling into the part faults.
3. **Guest sees 4096.**
   * `toybox getconf PAGESIZE` through the real `linker64` and `libc.so` prints `4096`.
   * `AT_PAGESZ` read back from `/proc/self/auxv` is 4096.
   * Both say the host page on the direct path.
4. **4 KiB ELF.** `linker64` `dlopen`s `libzstd-jni-1.5.7-6.so` from the APK, with no `pagecompat`.
   Its `PT_LOAD`s are mapped at `load_bias + p_vaddr` with file contents matching `p_offset`.
5. **Overlay life cycle.**
   * Split, then made uniform again: the page leaves the overlay and the alias is gone.
   * All parts unmapped: the host page is free.
   * A file view split: it is privatised and its bytes are kept.
   * A strict gap faults, and a lenient gap does not.
6. **The pager declines a split page at once** (no zero-commit streak).
7. **Regression on a 4 KiB host.** This machine can run it: an x86-64 build under Rosetta 2 has a
   4 KiB page (`target-x86`). The `omni-mem` and `omni-cpu` suites run there, and items 1, 2 and 5
   pass on the fast path without the overlay.

Then the full workspace suite on the M1. Any red test is reported by name.

## Milestones

* **M1 (correctness):** Units 1-4, the escape hatch and tests 1-7. Pass the gate on correctness:
  PS99 in-world past one minute, `libzstd-jni` alive.
* **M2 (performance), driven by M1's numbers:** compare in-world fps and served-trap counts with a
  Windows/Linux run. Only if the traps are hot, candidates in order of cost are:
  * guard-aware placement (put an unhinted `PROT_NONE`-then-commit reservation so its guard ends a
    host page);
  * a dynarmic patch that recompiles only the instruction that faulted on a split page onto the
    callback path (x64 untouched).

  Uniform pages are already on the fast path in M1: there is no "everything traps" stage to remove.

## Risks

* **The host GPU driver is handed raw guest pointers** (`gpu/mod.rs`). If one lands in a split page
  with a denied part, the driver faults in host code. This is mitigated by placement: unhinted
  mappings never share host pages, and Vulkan structs live in heap and stack pages. Such a fault is a
  host crash with a clear backtrace, so it would not be missed.
* **Trap cost on macOS** (three Mach exception round trips per served access) is unmeasured. M1
  measures it; M2 exists for it.
* **`vm_map_remap` behaviour on XNU** is pinned by Unit 1's probe before anything depends on it.
