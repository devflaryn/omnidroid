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
| ~~**Files**~~ — **DONE, phase 3b (D23)**: `omni_platform::fs`, a **rooted** descriptor table. Every guest path resolves inside one host directory the embedding supplies, and an instance with no root refuses every path call by name. Bionic's `FILE*` layer went to `omni-bionic` over a trait, as this row said it should | 29 `file-io` symbols of the reachable remainder (the row's "33" counted the whole classifier bucket, which includes symbols the initializers do not reach) |
| **Clocks** — monotonic and realtime now, and sleep | `clock_gettime`, `gettimeofday`, `gmtime_r`, `nanosleep`, `usleep`. The `Clock` trait `omni-bionic` already defines is the shape the adapter implements |
| **Process and environment** — pid, environment block, auxv, sysconf/sysinfo, abort/exit, cpu id, random bytes | 13 `process-env` symbols. `getauxval` is where the **`AT_HWCAP` decision** lands — still open, both arms measured, see the blockers table |
| ~~**Sockets and polling** — socket, poll/select, getaddrinfo~~ — **this row was wrong too, and phases 3d/3e are the correction**: `omni-platform` did **not** have to grow. `poll` and `select` need no OS call at all, because the descriptor space they observe is closed (the only bound symbols producing a descriptor are `open`, `__open_2` and `opendir`; `socket` and `eventfd` refuse), so every descriptor is a regular file, a directory or a standard stream and POSIX fixes the answer for all of them. `inet_ntop` and `gai_strerror` are pure computation and went to `omni-bionic`; the other four refuse by name. **Third row running whose OS-surface prediction was too large** | the 8 `network` symbols, **DONE** (D25) |
| **Process CPU time** — the one primitive these phases did need | `clock`, and with it `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`, whose phase-3a refusal this makes false. `GetProcessTimes` on Windows; Linux and macOS name `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` and are structural. **DONE** (D25) |
| **A log sink** | `__android_log_print`, `syslog`, `openlog`, `closelog` |
| ~~**Threads** — spawn, join, detach, attributes, scheduling~~ — **this row was wrong and phase 3c is the correction**: `omni-platform` did **not** have to grow. A guest thread is `std::thread` (portable), an `omni-mem` mapping for its stack, and an `omni-cpu` context whose TLS block satisfies D13 by construction. No new platform primitive exists, so there is no `unsupported` arm to write either — and D22's other half says fabricating one would be a false claim in the other direction | the 9 lifecycle/scheduling symbols, **DONE** (D24) |

**The five-target rule applies to every one of these.** When a platform primitive is added, add the
Linux and macOS signatures **at the same time** as honest `unsupported` returns naming the POSIX call
they intend to make. It costs minutes and it is what makes the non-Windows bring-up a fill-in rather
than a redesign. Do **not** write speculative `open`/`mmap` bodies for those targets — that was ruled
against deliberately, because an unverified body misbehaves silently where a typed error fails
immediately and visibly.

**CORRECTION (2026-09-21), and it is mine.** The table above was derived from the classifier's
*OS* buckets — `file-io`, `process-env`, `time-clocks`, `network`, `logging` — plus the nine
thread-lifecycle symbols. That treated `threads-sync` as already covered and ignored three buckets
entirely, so **seven reachable symbols had no phase at all**:

| missed | classifier bucket | why it fell through |
|---|---|---|
| `sigaction`, `sigfillset`, `raise` | `threads-sync` | signals share a bucket with the sync primitives, which were assumed covered |
| `longjmp` | `pure` | needs no OS, so it was never a candidate for an OS phase — and so was never assigned to any |
| `clock`, `time` | `time-clocks` | phase 3a enumerated five of the seven; these two were left behind |
| `mallinfo` | `memory` | returns 80 bytes through `X8` and would have to describe a libc heap that does not exist |
| `__gcov_dump`, `__gcov_flush` | `unclear` | the classifier refuses to bucket them, so nothing downstream did either |

Same failure shape as the withdrawn "88 already implemented" and the data-symbol substitution: a
derivation that looked complete because its **totals** were consistent, while its **membership** was
not. Derive the remainder by subtracting what is bound from the reachable set, and assert membership
rather than counts.

**The authoritative remainder was 51** when this was written, independently derived twice
(reachable 188 minus every symbol named in `bionic/handlers.rs` and `bionic/data.rs`). **Phase 3b
closed the first row and phase 3c the second, so it is 14 now**, and the adapter's own test asserts
that remainder as a set difference against the reachable file rather than as a total:

| group | n | symbols |
|---|---:|---|
| ~~**3b** file-io~~ | ~~29~~ | **DONE** — phase 3b, D23. The list was re-derived twice before anything was written and is exactly the `file-io` bucket of the remainder, as a set rather than a count: `__open_2 __write_chk access close closedir fclose fdopen feof fflush fgets fileno fopen fputc fputs fread fstat fwrite lstat mkdir open opendir pread read readdir rename rmdir stat statvfs unlink` |
| ~~**3c** threads + signals~~ | ~~8~~ | **DONE** — phase 3c, D24. Derived twice before anything was written, and it is exactly the plan's row: `pthread_create pthread_detach pthread_getschedparam pthread_join pthread_sigmask raise sigaction sigfillset`. Four answered, three refused by name (`sigaction`, `raise`, `pthread_sigmask`) and `sigfillset` implemented in `omni-bionic` because it is pure computation |
| ~~**3d** network~~ | ~~8~~ | **DONE** — phases 3d/3e, D25, run together as the remainder. Derived twice before anything was written and exactly the plan's row: `eventfd freeaddrinfo gai_strerror getaddrinfo inet_ntop poll select socket`. Two answered from `omni-bionic`, two implemented over the descriptor table `fs` already had, four refused by name |
| ~~**3e** the remainder nothing else claims~~ | ~~6~~ | **DONE** — D25: `clock time mallinfo longjmp __gcov_dump __gcov_flush`. `time` and `clock` answered, `mallinfo` and `longjmp` refused by name, and the two `__gcov_*` **not bound at all** — they are declared *absent*, so a weak reference resolves to null, which is what the guest's own `CBZ` expects and what a real device produces |

`longjmp` needs no OS but does need the boundary to restore a guest `jmp_buf`, which is why it is not
simply `omni-bionic` work. `mallinfo` is the only reachable import returning through `X8`.

**Both turned out to be refusals, and neither for a marshalling reason** (D25). `X8` *is*
marshalled — task 2 built `Args::indirect_result` for exactly this symbol — and the reason
`mallinfo` refuses is that `libroblox.so` imports no allocator at all, so there is no heap for the
answer to describe; eighty zeroed bytes is the believable wrong answer precisely because it is
arithmetically true of a libc heap nothing has allocated from. `longjmp` refuses because the
boundary deliberately gives a handler no way to write the calling thread's guest state (D18 makes
that a *type* property), and because `setjmp` is not among the 188, so nothing here can have
filled a `jmp_buf` in the first place.

**Task 3 is complete.** All 188 statically-reachable imports are accounted for: **143** answered,
**22** refused by name, **3** reporting a guest termination, **18** data objects and **2**
deliberately absent. D25 is the record, and the split is asserted by calling every symbol rather
than by counting a table.

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
