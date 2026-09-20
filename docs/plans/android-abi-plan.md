# Plan: the thunk boundary and bionic subset (milestone M3)

Spec: `docs/ARCHITECTURE.md` §5. Rationale: `docs/DECISIONS.md` D5, D7, D13, D16.
Builds on M2 (`cpu-execution`), where real `libroblox.so` functions execute correctly.

Scope: reach **M3** — all **3,594** `init_array` entries run to completion. They are what first call
imported symbols, so this milestone is the **thunk boundary** plus whatever bionic surface those
initializers actually reach. Excludes JNI (M4) and graphics entirely.

## Global Constraints

These carry forward because every one was earned. Constraints 11, 12 and 13 each caught real defects
in M2, including a measured use-after-free in a process-wide exception dispatcher.

1. **No fabricated implementations and no placeholder success.** A function that cannot do its job
   returns an error. No `todo!()`/`unimplemented!()` on a path a test claims to exercise. **A stub
   that returns a plausible value is worse than one that aborts** — 3,594 initializers will hide it.
2. **Tests use the real APK.** `Roblox-2.738.1397.apk` is the fixture; skip gracefully when absent.
3. **Exact values are mandatory** where given. 3,594 `init_array` entries; 565 imports in
   `libroblox.so`; 669 across all eleven libraries.
4. **Platform code is confined.** OS APIs and `#[cfg(target_os)]` only in `omni-platform`.
5. **`unsafe` is localized and justified**, stating what the *foreign* side guarantees. This milestone
   marshals between two ABIs, so a wrong invariant here corrupts silently.
6. **Commit charge is the scarce resource** (D10). Never commit speculatively.
7. **Errors are typed and diagnostic.** An unimplemented import must name *which symbol* at *which
   guest address*, because the failure will arrive 3,000 initializers deep.
8. **No network access at runtime.**
9. **Stable Rust.** Warnings from our own crates are a defect.
10. **Document discoveries.** M1 and M2 produced seven corrections to this project's own research;
    two would have caused silent corruption. Expect more.
11. **Hostile input is the expected case, and you test it yourself** (D6). Guest code is untrusted:
    it will pass null and wild pointers across the thunk boundary, lie about lengths, and re-enter.
    **A panic or abort reachable from guest-supplied arguments is Critical**, and an abort cannot be
    contained by any caller. A bound is only as trustworthy as its least-validated input, and
    saturating arithmetic on a limit turns hostile input into a larger permission.
12. **Every figure carries its sample size.** A measurement without its n is an anecdote formatted as
    a fact. One quantity in M2 was reported three times before settling at n=90, and two false
    figures shipped as code comments in the interim.
13. **A test that cannot fail is worse than no test.** Mutation-verify in **both** directions. M2's
    harness twice caught a fix that went too far, and once showed a fix was not load-bearing at all —
    that finding was then correctly withdrawn. Distinguish *exercising* code from *detecting* a bug:
    a counter that rises under load but stays at zero under the injected fault is a watch, not a
    detector, and must be labelled as one.
14. **A measured quantity appears once, with its n, and everything else links to it.** Three drifted
    duplicates have appeared in this project's own documents; every one was added later by someone
    summarising a result rather than measuring it.

---

## Task 1: Measure before building

The M2 whole-branch review named this the most valuable next step, and it is deliberately a task of
its own because both numbers *decide the design* of Tasks 2 and 3. Building first and measuring after
is how the 16 MiB-per-thread array and the 30-49x callback path were discovered late.

### The thunk round trip

The only cost figure on the branch is "**under 53 ns**" — and that is an **entry** ceiling which
explicitly excludes the exit and the re-entry. M3 needs the **round trip**: guest executes a branch
into the thunk region → the backend exits → the host services the call → the guest resumes.

Measure it with a stated n, and measure the alternatives, because this is the choice:
- **Exit to Rust per call** — simple, and costs the full round trip every time.
- **Dispatch inside the run loop** — the backend stays in generated code and the thunk resolves
  without a full exit, if the backend permits it.

Report both with sample sizes, then say which the runtime should use and why. Remember D16: a budget
above `i64::MAX` reads as negative and returns after every block, so do not pass one.

### What the initializers actually reach

`libroblox.so` imports **565** symbols, but the 3,594 initializers will touch a subset. Determine it
statically: from the `init_array` targets, follow the call graph across the `.eh_frame_hdr` function
bounds already recovered, and collect every import reachable from those entry points.

That set is Task 3's scope. Report it as a count and a list, grouped by providing library, and say
plainly whether the method over- or under-approximates — a static call graph over a stripped binary
with indirect calls is a lower bound on reachability, and saying so matters more than the number.

### Report
Both figures with their n; the dispatch recommendation with its reasoning; the reachable-import set
with its method's limitations stated; and anything either measurement revealed about the backend that
the M2 documents get wrong.

---

## Task 2: The thunk boundary

Bind the guest's undefined symbols to host implementations, in both directions.

- **Correction (Task 2): there was no thunk region.** This plan and its brief both said the loader
  already bound imports into a reserved region; it enumerated them but there was no region, no
  allocator and no provider, and building them was part of Task 2. That is the second time this
  plan described a design statement as existing code — check before relying on such a sentence.
- **AAPCS64 → host ABI marshalling.** Integer and pointer arguments in `X0`-`X7`, floating point in
  `V0`-`V7`, stack arguments beyond that, return values in `X0`/`X1`/`V0`. Variadics are the hard
  case and several libc functions need them.
- **Host → guest callbacks**, the mirror direction: a `pthread` entry point, an `atexit` handler, a
  comparator passed to `qsort`. The guest's function pointer must be callable from Rust.
- On ARM64 hosts the ABI already matches and the thunk reduces to near a direct call. Keep that path
  expressible even though it cannot be tested here (`ARCHITECTURE.md` §6, D5).
- An **unbound** symbol must fail with a typed error naming the symbol and the guest address — not a
  crash, and not a silent zero return.

### Tests
Round-trip every argument shape: integers, pointers, floats, mixed, more than eight arguments, a
variadic, a struct by value if any import needs it, and a callback in each direction. Then the hostile
cases: a guest passing a null or wild pointer, lying about a length, re-entering the boundary from
inside a callback, and branching into the middle of a thunk rather than its start.

---

## Task 3: The bionic subset the initializers need

Implement exactly the set Task 1 found — no more. `ARCHITECTURE.md` §5 is explicit that the import
list *is* the specification, and D7 established there is no JVM and no dex interpreter.

Guidance that is already established and should not be rediscovered:
- **`libroblox.so` imports no allocator at all** — corrected in Task 1, and the opposite of what an
  earlier draft of this plan said. It carries its own allocator and reaches the host through guest
  `mmap`, so the heap seam is the **demand pager**, not `malloc`. Do not implement `malloc`; check the
  reachable set for what the engine actually asks for.
- **TLS is `pthread_key_*` only** — no ELF TLS anywhere in this APK (D9). `TPIDR_EL0` is already
  programmed per guest thread with a bionic-layout block and a stack guard at `+0x28` (D13).
- **`dl_iterate_phdr` must be faithful, not a stub.** The C++ runtime is statically linked, so the
  in-guest unwinder walks 11.5 MB of `.eh_frame` through it, and C++ exceptions break without it.
- 23 imports are `STT_OBJECT` **data** symbols, not functions.

Anything you cannot implement correctly must fail loudly with the symbol named. A plausible-looking
stub is the worst outcome available, because the failure will surface thousands of initializers later.

### Tests
Each implemented function against its documented contract, including its error cases; the data symbols
readable; `dl_iterate_phdr` enumerating the real loaded image; and hostile arguments for every function
that takes a pointer or a length.

---

### Task 3 phase 3 — the OS surface, specified before it is built

**Derived, not guessed.** Method: classify the 188 statically-reachable imports with
`tools/os_surface.py` (the tool whose totals reproduce: 565 classified, exit 0), intersect with the
reachable set, then remove everything already implemented in `omni-bionic` by the strict test — a doc
comment naming the symbol **in backticks** above a `pub fn`. The loose version of that test is what
produced the withdrawn "88 already implemented"; the strict one gives 79.

The 188 classify as: `pure` 50, `threads-sync` 37, `file-io` 35, `data-object` **18**, `process-env`
15, `network` 8, `memory` 7, `time-clocks` 7, `dynamic-link` 5, `logging` 4, `unclear` 2. That sums to
188, and the `data-object` count of **18 independently reproduces D17's figure from a third tool**.

**63 of the 188 need OS surface `omni-platform` does not have**, plus the **9** thread-lifecycle and
scheduling symbols `omni-bionic` cannot own (D19) — **72 in total**. (The classifier files
`snprintf` and `vsnprintf` under `file-io`, but they format into a caller's buffer and are already
serviced; they are excluded from the 63. `fprintf`, `vfprintf`, `vasprintf` and `fscanf` are bound
and **refused by name** today, and are counted as blocked because each needs a destination that does
not exist yet.)

| `omni-platform` must grow | for |
|---|---|
| **Files** — open/close/read/pread/write, stat/fstat/lstat/statvfs, rename/unlink/mkdir/rmdir, opendir/readdir/closedir | 33 `file-io` symbols. Bionic's `FILE*` layer (`fopen`, `fgets`, `fputs`, `fflush`, `feof`, `fileno`, `fdopen`, `fread`, `fwrite`) then belongs in `omni-bionic` **on top of** the fd primitives, not in the platform crate |
| **Clocks** — monotonic and realtime now, and sleep | `clock_gettime`, `gettimeofday`, `gmtime_r`, `nanosleep`, `usleep`. The `Clock` trait `omni-bionic` already defines is the shape the adapter implements |
| **Process and environment** — pid, environment block, auxv, sysconf/sysinfo, abort/exit, cpu id, random bytes | 13 `process-env` symbols. `getauxval` is where the **`AT_HWCAP` decision** lands — still open, both arms measured, see the blockers table |
| **Sockets and polling** — socket, poll/select, getaddrinfo | 8 `network` symbols |
| **A log sink** | `__android_log_print`, `syslog`, `openlog`, `closelog` |
| **Threads** — spawn, join, detach, attributes, scheduling | the 9 lifecycle/scheduling symbols |

**The five-target rule applies to every one of these.** When a platform primitive is added, add the
Linux and macOS signatures **at the same time** as honest `unsupported` returns naming the POSIX call
they intend to make. It costs minutes and it is what makes the non-Windows bring-up a fill-in rather
than a redesign. Do **not** write speculative `open`/`mmap` bodies for those targets — that was ruled
against deliberately, because an unverified body misbehaves silently where a typed error fails
immediately and visibly.

**Sequencing note.** `dl_iterate_phdr` is *not* in this phase — it needs `omni-elf`'s loader state,
not the OS — and it is the highest-value single item in Task 3, because the statically-linked C++
runtime walks 11.5 MB of `.eh_frame` through it and **C++ exceptions break without it**.

---

## Task 4: All 3,594 initializers — milestone M3

Run them, in order, from the real loaded image.

### Tests — the M3 gate
- **All 3,594 complete**, counted, in order, with no fault and no unbound symbol.
- The count is asserted **exactly**, and the test fails if any initializer is skipped — a counter that
  only counts successes cannot tell completion from silence.
- Guest state after the run is inspectable and sane: something the initializers demonstrably wrote is
  read back and checked, so the gate proves they *ran* rather than that a loop terminated.
- Repeatable and leak-free: commit charge returns to baseline; the per-slice callback invariant from M2
  holds throughout (delta zero unless the slice ended in a memory-fault exit).
- Per-thread and total memory cost measured against the M2 figures, with n.
- Cold and warm timing for the whole initializer run, with n — this is the first figure that resembles
  a real startup cost.

### Report
Which imports the initializers actually called versus Task 1's static prediction — that comparison is
the most valuable output of this milestone, because it measures how well static reachability predicted
dynamic need, and M4 will rely on the same method. Plus timings, memory, and every discovery that
contradicts the existing documents.
