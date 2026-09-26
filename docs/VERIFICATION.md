# How verification fails here

Each entry is a real failure in this project where a green run proved less than it claimed. Code
cites the entry and rule numbers; keep them. Read this before trusting a test or a number.

## 1. A count cannot see a substitution

A list of 18 reachable data imports had two wrong members in and two right ones out; every
count-based assertion (and three confirmations of D17's figure) still passed.
Lesson: assert membership (a set difference, symbol by symbol), not totals.

## 2. A naive grep over-counts, and always in the flattering direction

"88 of 188 imports implemented" was really 79: the grep matched symbol names in doc-comment prose.
A regex over mutation ids gave 116 where parsing the list gave 118.
Lesson: state the method with every count; parse structure rather than match text.

## 3. `--release` hides overflow, and a wrapped value can satisfy the assertion

`gmtime` overflowed near `i64::MIN`; release builds wrapped, a later range check discarded the
garbage, and the regression test asserted nothing for 6 of its 10 inputs.
Lesson: use `checked_*` on guest-controlled arithmetic; detect profile-only defects in that profile.

## 4. A test that early-returns when a fixture is missing passes without asserting anything

The path-confinement test skipped when symlinks were unavailable (Windows `WinError 1314`), so two
of six confinement rules never ran; the M3 gate likewise returned `ok` without the APK.
Lesson: a missing fixture is a failure, not a skip; use an unprivileged equivalent (a junction).

## 5. The regression test for a previous defect is where the next gap hides

Both High findings of the adapter review were tests written as fixes for earlier bugs.
Lesson: when a fix lands, give it a mutation row.

## 6. A flaky test can launder itself into evidence

A racing condvar test inflated the catch list of an unrelated `wcslen` mutation; a CPU-clock test
failed because Windows charges process CPU in 15.625 ms ticks.
Lesson: make timing tests structural (a recording mock, a relation, a bounded poll), not
longer sleeps.

## 7. Never use a second implementation as your oracle

Rust's `Ipv6Addr` display disagreed with bionic's `inet_ntop` on 43 of 200,000 random addresses
(IPv4-compatible form).
Lesson: derive expected output from the specification (or the ARM ARM), never from another
implementation.

## 8. The harness misclassifies in both directions, and has done so twice

False `caught`: a command already failing on the clean tree marked 8 rows caught. False `MISS`: a
cargo lock race filed rows as "did not compile". Rows also went stale silently and ids collided.
Lesson: run the whole table (its pre-flights check every pattern); a total that does not add up
means an uncounted outcome.

## 9. Do not measure a rare event with a small sample

An `rwlock` stall read 1.0115 s once and 1.8 µs once; separating fixed from unfixed took 19,200
acquisitions per version (0.23% vs 0.010% stalls over 100 ms).
Lesson: every figure carries its n and method; count rare events over a large sample.

## 10. Verify the evidence, not just the conclusion

An adapter review found 14 decision-record statements true in outcome but wrong in reasoning
(e.g. "`__sF` is reached as `__sF + addend`": every data import's addend is zero).
Lesson: read the source, not a summary of it.

## 11. Exercising is not detecting

`sem_post` lost a wake (measured 1.0104 s to reach a blocked waiter), but timed retry slices healed
it, so every count-based test passed.
Lesson: a counter that stays zero under the injected fault is a watch, not a detector; label it.

## 12. A branch no input can take is not a check

`ALooper_release` guarded `references < 0`, which no input could reach; writing a test for the
guard exposed it.
Lesson: delete unreachable guards or make them `debug_assert!`; if a test can't be written, ask if
the check can fire.

## 13. An `expect`'s justification can expire, and a comment is not what holds it

Eight `expect("the slot was checked live")` sites re-locked after `looper_at` had dropped its lock,
so a guest release in between became a host panic inside an import; found by reviewing a copy.
Lesson: the check and the use must sit under the same lock; say what makes an `expect` hold.

## 14. A test can assert the defect, and a comment can supply the reasoning that makes it look right

`strchr` had no terminator arm; a test asserted the resulting fault, with a comment paraphrasing C
wrongly. OpenSSL's name map hit it under a global lock and hung client settings.
Lesson: cite the clause (`C 7.24.5.2`) rather than paraphrase; trace what an open failure holds.

## 15. A diagnostic that can be switched off looks exactly like a system that has stopped

`Boundary::census` was off after `report()`, so a watchdog read `FROZEN` for three sessions while
`Boundary::crossings().exits` showed 1.3 M imports/s.
Lesson: a switchable number says so in its output; pair it with a counter nothing can switch off.

## 16. A thread that dies is not a call that fails

The gate passed while this layer killed three guest worker threads (unbound symbols, a bounded
`cstr` walk); the first assertion on it found a fourth (`strcspn`) on the default path.
Lesson: `Bionic::guest_thread_failures()` is a result; assert it empty and make first
encounters loud.

## 17. `test result: ok` is a line the process prints before it has finished

`jni_startup` printed `2 passed`, then exited `0xc0000005` because guest threads outlived the
address space during teardown.
Lesson: the process exit code is the verdict; stop and join guest threads before teardown.

## 18. A shared build directory runs the other tree's code

Worktrees sharing `CARGO_TARGET_DIR` overwrote each other's test binaries; `--test bionic`
reported `216 passed` from another tree's source.
Lesson: a result is about a binary; use private target dirs and check `-- --list` for your test.

## 19. A capture that finds nothing is not evidence that nothing was drawn

`PrintWindow` (even `PW_RENDERFULLCONTENT`) returned one colour while the census showed Vulkan
presents; the call log's `MAX_RECORDS` cap also dropped 4,867 calls.
Lesson: show an instrument seeing a known case before it may report "nothing".

## 20. A test that builds its own input tests a caller that does not exist

`showKeyboard`'s test passed resolved objects, but `read_varargs` passes raw handles, so the first
live run saw a real array as null.
Lesson: build a unit's input the way its real caller does (`new_local`, `reference_to`).

## 21. A stimulus aimed at a screen is only as good as the capture it was aimed from

Taps at Sign In `(640, 397)` worked only after a resize; without one the layout sits 30 px higher.
Lesson: aim a synthetic tap from a capture of the same configuration and capture after it.

## 22. A pinned value can be a record of a race, and a number in range is not a pointer

The M3 gate flaked: a pinned word was an emutls index set by a thread race, and "pointer into the
image" counts caught u32 pairs that lay in range for some placements.
Lesson: find the code that writes a pinned value; tell pointers by a second placement, not by value.

# Global Constraints

Code cites these as "Global Constraint N" (numbering of the M3 plan, carried forward; the old
milestone plans are deleted, git holds them).

1. No fabricated implementations or placeholder success: a function that cannot do its job
   returns a typed error.
2. Tests use the real APK (`Roblox-2.738.1397.apk`, git-ignored). A missing APK fails (entry 4).
3. Exact values where known: 3,594 `init_array` entries and 565 imports in `libroblox.so`; 669
   undefined symbols across all eleven arm64 libraries (`omni-elf/tests/all_libraries.rs`;
   measured with the modified APK's `libzstd-jni`, not re-measured on stock).
4. Platform code is confined: OS APIs and `#[cfg(target_os)]` only in `omni-platform`.
5. `unsafe` is localized and states what the foreign side guarantees.
6. Commit charge is the scarce resource (D10): never commit memory speculatively.
7. Errors are typed and diagnostic: an unimplemented import names the symbol and guest address.
8. Withdrawn by D30 (was: no network at run time).
9. Stable Rust; warnings from our crates are defects.
10. Document discoveries, including where the research was wrong.
11. Hostile input is the expected case (D6): a panic or abort reachable from guest input is
    Critical; guest code is untrusted by design.
12. Every figure carries its sample size.
13. A test that cannot fail is worse than no test: mutation-verify both directions (A reverts the
    fix, B over-corrects).
14. A measured quantity appears once, with its n; everything else links to it.

# Process rules these produced

1. **Commit in pieces.** Agents get cut off by usage limits; uncommitted work is what is lost.
2. **Never `git add -A` or `-a`.** Stage explicit paths and read the staged diff (not just
   `--numstat`) before committing: a mutation has been committed as source twice.
3. **Nothing else touches the tree while mutations are live**, in both directions: no cargo, edits
   or git during `tools/mutate.py`, and hand-applying a mutation or importing a row file that runs
   at import time counts as a harness run. Row files must be pure data. Afterwards,
   `git diff --exit-code` the source directories.
4. **Let a killed harness run exit; confirm the process is gone.** Its restore is a `finally`. One
   clean `git status` proves nothing, since the tree goes clean between rows.
5. **Add rows without slicing the table, and check ids are free.** Windows rows go in
   `tools/mutate.py`'s `MUTATIONS` (insert before the terminator), macOS rows in
   `tools/mutate_mac/*.py` (`mac-`), Linux rows in `tools/lnx_rows/*.py` (`lnx-`, run by
   `tools/mutate_linux.py`). The harness refuses duplicate ids before running anything.
6. **A figure enters `DECISIONS.md` only after someone other than its author reproduces it.**
7. **Parallel agents use worktrees with private target directories and disjoint files named in
   their brief.** Each branch is reviewed as a diff; the merged tree is tested before committing.

## The harness (`tools/mutate.py`)

`--only <prefix>` selects rows by id prefix; `--list` prints them. Before mutating anything it
refuses duplicate ids, checks every selected pattern matches exactly once, and runs each distinct
command on the clean tree (it must pass). A row that does not compile is retried once; a crash is
re-run with `--test-threads=1` to name the test. `MISS` and `NOT CAUGHT` are never passes.
