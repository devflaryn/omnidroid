# How verification fails here

Every entry below is a real failure from this project, not a general principle. They are written down
because each one produced a **green suite that proved less than it claimed**, and in most cases the
person who wrote the test was the person who wrote the code — which is the blind spot that makes all
of them possible.

There are **thirteen** of them. Entry 12 arrived in M5 and is the only one found by a test that
could not be written rather than by one that passed. Entry 13 arrived in M6 and is the only one
found by reviewing a *copy* of the defect rather than the original.

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

---

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
4. **Let a killed harness run exit rather than killing it again.** Its restore is a `finally`; a
   killed interpreter skips it and leaves a mutation live.
5. **Edit `tools/mutate.py` by inserting before the list terminator, never by slicing it.** Slicing
   truncated the file once. Check your ids are free before adding — ones that look free have not been.
6. **A figure enters `DECISIONS.md` only after someone other than its author reproduces it.** That
   rule has now caught seven wrong numbers.
