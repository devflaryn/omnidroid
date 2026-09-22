# How verification fails here

Every entry below is a real failure from this project, not a general principle. They are written down
because each one produced a **green suite that proved less than it claimed**, and in most cases the
person who wrote the test was the person who wrote the code — which is the blind spot that makes all
of them possible.

There are **sixteen** of them. Entry 12 arrived in M5 and is the only one found by a test that
could not be written rather than by one that passed. Entry 13 arrived in M6 and is the only one
found by reviewing a *copy* of the defect rather than the original. Entry 14 arrived in M6 and is
the only one where a **test asserted the defect** and a comment supplied the reasoning that made
it look right. Entries 15 and 16 arrived together, later in M6, and are the only pair where the
*instruments* were wrong rather than the code: a diagnostic that had been switched off read exactly
like a system that had stopped, and three guest threads died unremarked behind a green gate.

**This count has itself been wrong.** It said "fourteen" for as long as there were sixteen,
because two entries were appended without it — which is the same defect as an `expect` whose
justification expired (entry 13), in the document that warns about that. If you add an entry, the
number above is part of the entry.

Read this before writing a test you intend to rely on, and before believing a number.

---

## 1. A count cannot see a substitution

The reachable `STT_OBJECT` list named `timezone` and `tzname`, which the initializers never reach,
and omitted `AMEDIAFORMAT_KEY_STRIDE` and `AMEDIAFORMAT_KEY_WIDTH`, which they do. **Two wrong in,
two wrong out, total unchanged at 18** — so every count-based assertion passed, including D17's
figure, which had been independently confirmed three separate ways.

Three confirmations of the *number* and none of the *membership*.

> **Assert membership, not totals.** A set difference against the source of truth, named symbol by
> symbol. `assert_eq!(list.len(), 18)` is not a test of a list.

## 2. A naive grep over-counts, and always in the flattering direction

"88 of the 188 imports are already implemented" was wrong; the real figure was **79**. The method
matched a symbol anywhere in a doc comment, so English prose scored as implementation — `abort`,
`access`, `read`, `stat`, `raise` and four others. Wrong by nine, and in the direction that made the
remaining work look smaller.

The same shape recurred twice more: a regex over mutation-row ids gave 116 where an AST parse of the
list gave 118, and a phase-3 plan derived from classifier buckets missed seven reachable symbols
because they sat in a bucket assumed to be covered.

> State your method with any count, and prefer parsing the structure to matching its text. When a
> derivation and a direct measurement disagree, the derivation is wrong.

## 3. `--release` hides overflow, and a wrapped value can satisfy the assertion

`gmtime` computed `timestamp - days * SECONDS_PER_DAY`, which overflows near `i64::MIN`. In debug
that panics. In release — which is what `cargo test --workspace --release` runs — **it wraps, the
garbage is discarded by a later range check, and the observable behaviour is correct by accident.**

The regression test written to close it asserted nothing for six of its ten inputs, because they all
took an empty `Err` arm. So the whole suite stayed green with the Critical live, and *no assertion
could have caught it*. The detector had to be a mutation row, run in debug.

> Check arithmetic on guest-controlled values with `checked_*`. And when a defect is only visible in
> one build profile, the detector belongs where that profile runs.

## 4. A test that early-returns when a fixture is missing passes without asserting anything

The path-confinement test skipped when the host could not create a symbolic link. This host cannot —
**MEASURED: `WinError 1314`, required privilege not held.** So two of the six confinement rules, the
security property of the whole filesystem seam, had **never executed**, on the machine whose green
suite was the evidence for them. Neither had a mutation row.

The M3 gate had the same shape: it returned early when the APK was absent, printing a notice — but a
skipped test still reports `ok`, and that test *is* the milestone's evidence.

> A fixture that is missing is a **failure**, not a skip. If a test cannot run, it must say so by
> failing. Where a privileged operation is unavailable, find an unprivileged equivalent — a Windows
> **directory junction** needs no privilege and is a reparse point `is_symlink` reports true for.

## 5. The regression test for a previous defect is where the next gap hides

Both High findings from the adapter review were **tests written as the fix for an earlier bug**.
Nobody re-audits those: a test that closed a defect is assumed to work, and its existence is taken as
proof the defect cannot return.

> When a fix lands, mutate the fix. A regression test with no mutation row is an assertion that
> something used to be broken, not evidence that it still gets caught.

## 6. A flaky test can launder itself into evidence

`cond::tests::signal_wakes_exactly_one` raced: its waiters' 400 ms timeouts could fire inside the
main thread's 300 ms observation window once registration took ~100 ms, which it does under load. It
was seen three times — twice as a red run, and **once inflating the catch list of `bionic-A6`, a
`wcslen` mutation.** A wide-string change cannot break a condvar.

So a flake does not merely cost a red run: **it can make a mutation row look detected when nothing
detected it.**

A sixth flake, from M5, has a different cause and the same remedy: **a quantised clock**.
`clock_is_process_cpu_time_in_microseconds_rather_than_wall_time` asserted that three million guest
instructions moved the process CPU clock, and it failed once in a whole-workspace run —
**MEASURED: `1843750 -> 1843750`**. Windows charges process CPU in 15.625 ms scheduler ticks, and
that figure is exactly 118 of them; three million guest instructions do not reliably cross a tick
boundary. So the assertion was really *this pass happened to straddle a tick*. It now repeats the
work until the clock moves, under a wall-clock deadline.

> Timing-dependent tests get made structural — a recording mock, an asserted relation between two
> constants, or a bounded poll on the thing under test — not given a bigger sleep. Four flakes in
> this layer were fixed this way; the fifth was found by the harness refusing to run; the sixth was
> a clock whose resolution was coarser than the thing being measured.

## 7. Never use a second implementation as your oracle

`inet_ntop` must match **bionic's** output. Rust's `Ipv6Addr: Display` looks equivalent and is not:
**MEASURED, 43 disagreements in 200,000 pseudo-random addresses**, all one class — Rust stopped
printing the deprecated IPv4-compatible form in dotted notation, where BIND (which bionic ships)
still does.

The same trap is recorded for `rand`, which is an LCG approximation rather than bit-exact bionic, and
is safe **only because nothing validates against bionic's exact sequence**.

> Derive expected output from the specification. Agreeing with another implementation proves only
> that you both did the same thing.

## 8. The harness misclassifies in both directions, and has done so twice

- **False `caught`.** A test command that already failed on the *unmutated* tree made every row using
  it report `caught` regardless of what the mutation did. Eight results were worth nothing.
  Fixed by a pre-flight that runs each distinct command on the clean tree first — `11/11 commands
  pass` is now printed before anything is mutated.
- **False `MISS`.** A transient build failure — a cargo lock or filesystem race — was filed as
  "did not compile", indistinguishable from a genuinely broken row. **MEASURED: one 298-row run
  produced two, and both compiled fine and were caught on retry; one logged `did not compile (2s)`
  where its real build and suite take 23s.** Fixed by retrying once: a mutation that truly does not
  compile fails twice.

Also found: **six rows silently staled** when a large feature moved the code they anchored on, and
**an id collision** (`plat-A1`..`A4` reused) whose totals still looked right — entry 1, inside the
tool built to catch entry 1. The harness now refuses duplicate ids.

**The staling recurred in M5**, which is why the rule below is in the imperative: rewriting `poll`
and `select` to answer from a real readiness source staled **four** `net-*` rows, and `--only pipe`
and `--only looper` were both green while it was true. The whole-table *pre-flight* — every pattern
matched, nothing run — costs a second and is what found them.

> Run the **whole table**, not just the rows you added. Both harness defects were found that way and
> neither would have surfaced otherwise. And a total that does not add up — `297/298 caught, 0 NOT
> CAUGHT` — means an outcome category you are not counting.

## 9. Do not measure a rare event with a small sample

An `rwlock` stall measured **1.0115 s** in one run and **1.8 µs** in another, which looked like a
500,000× improvement and was noise: the *unfixed* build also measures 1.5–3.5 µs in most runs. Eight
runs per version separated them not at all.

The honest figure needed **19,200 acquisitions per version**: 44 stalls over 100 ms (0.23%) against
2 (0.010%), worst case 2.0169 s against 119.7 ms.

> Every figure carries its **n** and its method. For a rare event, count occurrences over a large
> sample rather than reporting a best or worst observation.

## 10. Verify the evidence, not just the conclusion

Several claims in this project's own decision records were true in outcome and wrong in reasoning:

- `declare_data` justified requiring a size with "`__sF` is reached as `__sF + addend`". **Measured:
  all eighteen data imports have exactly one relocation, `R_AARCH64_GLOB_DAT`, every addend zero.**
  The conclusion survived for a different reason.
- `civil_from_days` was described as "branch-free over the whole `i64` range". `floor_div` alone has
  an `if`/`else`. The load-bearing property is **loop-free**, and the stronger word would invite a
  constant-time claim it cannot support.
- Two files were said to be separate "because the implementations genuinely differ in shape". Both
  were **identical one-line re-exports**.
- The `AT_HWCAP` decision was framed as *advertise → 53 hard halts into the interpreter*, which reads
  as fatal. **D5's own risk 4 says the opposite in its own words**: unimplemented decoder entries
  "surface cleanly via `InterpreterFallback` at ~87 ns per trap, so they are correct but slow". That
  single line turned an apparently forced choice into a measurable one (D26).

An adapter review found **fourteen** such statements across four decision records. The implementations
were largely sound; the *evidence* for them was weaker than the records claimed.

> Read the source, not the summary of it. A summary that has been through one restatement is where
> the reasoning quietly inverts.

## 11. Exercising is not detecting

> A counter that rises under load but stays at zero under the injected fault is a **watch**, not a
> detector, and must be labelled as one.

`sem_post` consumed the waiter flag that other waiters needed, costing a **measured 1.0104 s** for a
posted token to reach a blocked waiter. Every test passed: `sem_wait` loops on a bounded timed slice,
so a lost wake always *eventually* healed and every assertion about tokens and counts still held. The
suite exercised the wake path thoroughly and never detected that the wake did not happen.

The detector had to assert on **latency and on the flag word**, not on the final count.

## 12. A branch no input can take is not a check

`ALooper_release` guarded on `references < 0` and refused, with a paragraph explaining why
saturating would be worse. **The guard was unreachable.** The count starts at one, the slot is
freed the moment it reaches zero, so nothing can ever observe it below — the refusal that does the
work is the identity check one line earlier, which reports the slot as not live.

It was found by writing the test *for the guard*, which could not construct an input that reached
it. Nothing else would have: the branch compiles, reads as careful, and every suite around it was
green. A reviewer scanning for missing checks would have counted it as present.

The same shape is worth watching for wherever a defensive branch sits **after** something that
already makes its condition impossible — a bound checked twice, a null tested after a
dereference, a state guarded after the state machine has left it.

> Delete an unreachable guard rather than keeping it as reassurance. If it is worth keeping as a
> statement, make it a `debug_assert!` — which says *this cannot happen* — not an `if` that says
> *this might*. And when a test for a check cannot be written, ask whether the check can fire
> before assuming the test is hard.

## 13. An `expect`'s justification can expire, and a comment is not what holds it

Eight sites across `ndk/looper.rs` and `ndk/window.rs` read `state.loopers.get_mut(at).expect("the
slot was checked live")`. The check was real and one call earlier. **But `looper_at` took the
lock, checked, and dropped the guard before returning**, and every caller then re-locked. The
justification was true when it was written down and false by the time it was used, and the only
thing carrying it across the gap was the sentence inside the `expect`.

What it cost is the point: a guest that releases a handle it still holds is a *guest* defect, and
this layer's entire contract is that such a thing becomes a typed refusal naming the symbol and
guest address. This turned it into a **host panic unwinding out of an import** — the one failure
this project is built to never produce, sitting behind a string that asserted it could not happen.

Every suite was green, and would have stayed green: tests drive one guest thread per handle, so
nothing in them opens the window. It was found by **reviewing a new module that copied the pattern
faithfully** — `window.rs` inherited it from `looper.rs` along with the comment. That is entry 5
one step along: the defect became visible in the copy, not in the original, because a second
instance is the first time the reasoning is read rather than remembered.

And it was not purely a race. `ALooper_pollOnce` reaches it **single-threaded**: the registered
callback is guest code, invoked with no lock held, and `ALooper_release` is one of the things it
may call before returning the 0 that asks for its own registration to be removed.

> An `expect` is justified by structure, not by prose. If the justification is "this was checked",
> the check and the use must be under the same lock, in the same scope, with nothing between them
> that could yield — and if they are not, the sentence is documentation of a bug. Where the
> justification genuinely holds, say what makes it hold ("checked live under this same lock"), so
> the next reader can verify the claim instead of trusting it.
## 14. A test can assert the defect, and a comment can supply the reasoning that makes it look right

`strchr` had **no terminator arm**. It read forward until it found the byte or faulted, so a
search for a byte the string does not contain walked past the terminator and kept going until it
left the mapping. C 7.24.5.2 searches *the string pointed to by `s`* — the bytes up to and
including its terminator — and returns a null pointer when the byte is absent, which is the whole
of how every caller detects absence.

The suite was green, and it was green **because a test asserted the defect**:

```rust
// 'z' is absent, so the scan walks to the NUL and past it — C says strchr scans until
// it finds the byte, so absence inside a 6-byte mapping means the scan hits unmapped
// memory after the terminator: a fault (not guest NULL) is the honest result here.
assert_eq!(strchr(&mem, 0x1000, 'z' as i32), Err(Fault(0x1006)));
```

Three things made it survive. The assertion was **specific** — a fault at an exact address, which
reads as a measured fact rather than a guess. The comment **supplied a rule** ("C says strchr
scans until it finds the byte") that is false and that nobody had to check, because it was stated
in the voice this project states measurements in. And the behaviour is **plausible**: "the string
is unterminated, so a fault is honest" is true of `strchr`'s *sibling* cases and of the second
test in the same file, which maps eight bytes with no NUL at all and correctly still faults.

> A comment is not a citation. When a test asserts what a C function does, the doc comment beside
> it should name the clause — `C 7.24.5.2`, `POSIX strchr` — not paraphrase it, because a
> paraphrase is exactly as convincing when it is wrong.

**What it cost is the point, and it is the widest blast radius of anything in this file.**
`libroblox.so` embeds OpenSSL, whose `crypto/core_namemap.c` tokenises an algorithm-name list by
calling `strchr(names, ':')` in a loop at guest `0x029f7748`; most names contain no colon. The
scan left the allocation, the refusal unwound out of OpenSSL **while it held a global lock**, and
every later acquirer of that lock spun for ever. That lock is taken by `nativeInitClientSettings`,
so §8 row 21 hung; the engine never received flags; it never asked for a renderer. From the
outside, a **one-line omission in a search function** presented as *graphics do not work* — four
milestones and one subsystem away from the cause, in a subsystem that had never been asked for
anything.

The route to it is worth keeping too, because none of the obvious steps found it:

* `sched_yield` at 22 million calls, `parked`, `guest_thread_failures` and the import census all
  said "nothing is running" and none said why.
* Three hypotheses were **eliminated by measurement** and each would have been a plausible place
  to stop: `STLR` writes (tested at the CPU level), nested guest calls preserve every
  callee-saved register (read from `SavedState`), and no wake ever landed near a waiter (a
  near-miss detector added for the purpose, which reported zero).
* The step that found it was correcting a **broken measurement**. The lock value printed beside
  each scripted downcall was read *after the whole table had run*, so every row reported the same
  number and the lock looked as though it had been held since step 7. Running the table one row
  at a time, reading the lock between rows, named the exact call that takes it and never gives it
  back — and that call was the one already known to fail, carried in the handoff for two sessions
  as an open item whose consequences were never traced.

> **An open item with no consequence attached is a bet that it has none.**
> `nativeSetPlatformHeadersWithIdfa` was recorded as "20 of 21 downcalls return; this is the one
> that does not", milestone after milestone, without anyone asking *what it was holding when it
> stopped*. A refusal is not inert: it unwinds out of guest code at whatever point the guest had
> reached, and if that point is inside a critical section the guest never leaves it. Every
> refusal that can fire inside a lock is a deadlock waiting for the second acquirer.


---

## 15. A diagnostic that can be switched off looks exactly like a system that has stopped

`Boundary::census` keeps its per-symbol counts when the flag is cleared -- deliberately, so that a
run which counted and then stopped can still report what it counted. The gate's watchdog read the
total every twenty seconds and printed `FROZEN` when it did not move.

It did not move for three sessions, and the census had simply been **off**: `report()` stops it to
print a stable snapshot, and every interesting thing in M6 happens after `report()`. The reading
was not stale, not slow and not deadlocked. It was not a reading at all.

Everything downstream inherited it. `last_call` is written by the same counter, so that froze too
and named `memset` for twenty minutes. The per-thread crossing records are written by the same
counter, so every thread reported "stopped". A hypothesis was built on all three agreeing.

What broke it was a counter that is **not** gated: `Boundary::crossings().exits`, charged in the
same function as the census on the same line of control flow. One reading of it said 26,631,317 ->
53,276,895 -> 79,473,983, and the guest that had been "deadlocked since step 7" was executing
1.3 million imports a second.

> A number that can be *off* must say so in its own output, or it will be read as a measurement of
> the thing it is not measuring. Where that cannot be arranged, keep one counter in the same place
> that nothing can switch off, and read the pair -- two counters that must agree are a check, and
> one counter is a belief.

## 16. A thread that dies is not a call that fails

The gate reported, run after run and correctly: all 3,594 initializers, `JNI_OnLoad`, **21 of 21**
scripted downcalls, all seven lifecycle rows returned, the flags loaded. Behind it, three of the
guest's own worker threads had been **killed by this layer** and nothing printed a word about it:

* one on a perfectly ordinary, NUL-terminated log line that `GuestMem::cstr` refused because its
  walk was bounded by a commit-granule boundary (see `omni_mem::scan_reach`);
* one on `pthread_getattr_np`, unbound;
* one on `pthread_mutex_trylock`, unbound -- whose primitive `omni_bionic::mutex::trylock` already
  existed and was already unit-tested, and had simply never been wired to a symbol.

Each took with it whatever the guest had given it to do. One was holding the future that
`nativePostClientSettingsLoadedInitialization3` was waiting on, which is why the runtime hung.

Every assertion the gate makes is about a call **this** thread made. A guest thread that starts,
runs, and dies is invisible to all of them: it is not a downcall that returned an error, it is not
a refusal on the calling thread, and `live_guest_threads()` going down is indistinguishable from a
worker finishing its job.

> `Bionic::guest_thread_failures()` is not a debugging aid, it is a **result**. Print it after
> every step, and assert it empty wherever the run is expected to be healthy. A harness that only
> checks the thread it is standing on will report a green run on a burning building.

The assertion earned itself on its **first** run. It was added to catch the three deaths above, all
of which needed `OMNI_M6_ROWS_21_22=1` to reach — and it immediately failed the gate's *default*
path on a fourth, `strcspn`, which had been killing a guest thread on every ordinary run of this
suite for as long as that thread has existed, in a build everybody called green.

There is a second lesson under the first. Four of the five symbols involved
(`pthread_mutex_trylock`, `pthread_attr_getstack`, `__strcat_chk`, `strcspn`) were **already
implemented and already unit-tested** in `omni-bionic`; only the line wiring each to its symbol was
missing. The primitives had been written against the *import list* and the wiring done against
*what the run had reached*, so every primitive whose symbol the run had not yet touched sat there,
tested, unreachable and invisible.

> Two lists that are built from different sources will disagree, and the disagreement will be
> silent in whichever direction nothing asserts. If one side is generated from a specification and
> the other from experience, something has to assert that every entry on the first side is on the
> second — or, failing that, something has to make the first *encounter* loud.

# Process rules these produced

1. **Commit in pieces.** Agents have been cut off mid-task by usage limits repeatedly; uncommitted
   work is the only thing at risk. One survived a limit only because it had committed thirteen times.
2. **Never `git add -A` or `-a`.** Stage explicit paths and read `git diff --numstat` first: a
   one-line change in a file you never touched is the signature of a live mutation. **A mutation
   has been committed as source here twice.** The second time the path was explicit and the file
   was one the committer had genuinely edited seconds earlier -- so naming the path is necessary
   and is *not* sufficient. What would have caught it: reading the staged diff, not the numstat.
   The hunk said `expect` while the comment three lines above it, and the commit message, both
   said `if let`. **When the diff disagrees with the prose you just wrote, the diff is the tree
   and the prose is your intent.**
3. **Never run cargo, edit a file, or run git while the harness is running** -- and the
   prohibition is **symmetric**. It mutates the tree in place. A concurrent build already cost one
   discarded full-table result, and the number would have been believable.

   The symmetry is the second lesson, and it cost the second committed mutation. Anything that
   hand-applies a row is a harness run, whether or not it is `tools/mutate.py`: an agent proving a
   detector by reverting a fix holds the tree exactly as the harness does, for seconds at a time,
   with no lock and no announcement. The other party read rule 3 as binding only the harness
   operator, edited and committed the same file inside that window, and captured the reversion.

   > Before dispatching work that hand-applies mutations, decide who owns the tree for its
   > duration -- and do not be the one who commits. If a file must be shared, the mutator restores
   > byte-for-byte and says so with a hash; the committer reads the staged diff before every
   > commit. Both halves happened here, which is the only reason the correct bytes were never
   > lost.

   And a third angle on the same rule, met immediately afterwards: **importing a proposal is
   running it.** An agent's row file was imported to read its `ROWS` table, and its verification
   loop was not behind `if __name__ == "__main__"` -- so the import applied all seven mutations to
   the working tree and ran the suite. It restored correctly, from a `finally`, and the tree was
   checked byte-identical against `HEAD` straight after. The habit that makes that check automatic
   is the whole defence: **after anything that could have touched the tree, `git diff --exit-code`
   the source directories before doing anything else.**
4. **Let a killed harness run exit rather than killing it again.** Its restore is a `finally`; a
   killed interpreter skips it and leaves a mutation live.

   **And confirm what actually died.** A background *shell* was reaped here for system memory
   pressure, and the harness it had launched **kept running as an orphaned child**. The tree was
   checked immediately, came back clean, and that was believed. It was clean because the run was
   *between rows* -- minutes later a probe found `jmid-B1` live in `jni/classes.rs`, and minutes
   after that the tree was clean again because the run had finished normally and restored.

   > One clean `git status` after a kill is not evidence the harness is gone; it is a sample of a
   > tree that goes clean and dirty every few seconds by design. Confirm the **process** is gone,
   > or wait for its log to print its own final tally, before believing either the tree or the
   > results.

   The second half of that hour was worse and was avoidable: believing the run was dead, cargo was
   run and a file mutated **while it was still going** -- rule 3 again, from the other side, hours
   after rule 3 was amended for the same mistake. Both runs then compiled the same crate from
   different source states behind one cargo lock. Nothing was lost, and the results of the
   overlapping prefix are worth exactly as much as that: they have to be run again on a tree
   nobody else is in.
5. **Edit `tools/mutate.py` by inserting before the list terminator, never by slicing it.** Slicing
   truncated the file once. Check your ids are free before adding — ones that look free have not been.
6. **A figure enters `DECISIONS.md` only after someone other than its author reproduces it.** That
   rule has now caught seven wrong numbers.
