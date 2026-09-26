# Sub-project A: the Linux kernel personality (real bionic and linker64)

Date: 2026-09-27. Status: design, awaiting the owner's review.

## 0. Why, and where A sits

**The goal (the owner's words, 2026-09-27).** Any Roblox APK, including future versions, runs
without anyone updating omnidroid for it. Every `classes*.dex` is loaded into the app's class
loader and runs; every `.so` is loaded dynamically when code asks for it, as on a real device.
Nothing is transcribed or hardcoded per Roblox version.

**What that reverses.** D7 ("no JVM, no ART, no dex interpreter") and the README constraint "run
only the Android surface Roblox actually uses". The transcription (`jni/surface.rs`,
`jni/classes.rs`) and the function-level bionic (322 bound symbols) are per-version by
construction: each Roblox update can reach a Java body or an import nobody wrote. Recorded as
**D39** when this spec is approved.

**The approach (chosen 2026-09-27).** Run the real AOSP userspace as guest code on omnidroid's
existing CPU, memory, graphics, audio and window layers. omnidroid stops emulating libc and
emulates the Linux kernel instead. What can still need work after a Roblox update is then an
Android API or kernel feature this layer lacks, fixed once in generic code, never an answer
specific to Roblox.

**The four sub-projects**, each with its own spec, plan and build:

| | Sub-project | Done when |
|---|---|---|
| **A** | Linux kernel personality: real bionic + `linker64` run AOSP programs | A1-A5 (section 1) pass on Windows |
| B | ART: `dalvikvm64` runs a `.dex` against the real boot classpath | a hello-world dex prints |
| C | Binder + minimal system services; Roblox's `Application`/`Activity` boot from all its dex through a `PathClassLoader`; `System.loadLibrary` loads its `.so` files through the real `linker64`; GameActivity gets its surface | the engine's `JNI_OnLoad` is reached from Java, not from a script |
| D | Roblox in a world on the new path; parity with the current path; transcription and function-level bionic retired | the owner plays; README, ARCHITECTURE, STATUS updated |

The current Roblox path is untouched until D.

**The binaries.** A prebuilt, Apache-licensed `aosp_arm64` build for **Android 15 (API 35)**
(Roblox's `targetSdkVersion`) from ci.android.com, no Google apps, pinned by sha256 as the APK
is. No AOSP source build until a patch is unavoidable.

## 1. Scope

**Goal of A.** A real AOSP 15 arm64 program starts as the Linux kernel starts it on a phone:
`linker64` as the ELF interpreter, the real bionic `libc.so` initialising itself (TLS,
properties, scudo), then the program's own code. Nothing on this path is emulated at libc level.

**Milestones**, each a gate test against binaries straight out of the pinned image:

| | Proof | What it forces |
|---|---|---|
| A1 | `toybox echo hello` prints `hello`, exit 0 | `PT_INTERP` load, initial stack + auxv, `linker64` self-relocation, real `mmap`/`MAP_FIXED`, TLS, `set_tid_address`, `write`, `exit_group` |
| A2 | `toybox ls -l /system/lib64`; `cat /proc/self/maps`; `toybox ps` shows itself | `getdents64`, `readlinkat`, symlinks, `newfstatat`/`statx`, synthesized `/proc` |
| A3 | `getprop ro.build.version.sdk` prints `35` | a real bionic property area under `/dev/__properties__`, built from the image's `build.prop` files |
| A4 | the `threads` fixture (section 5) passes | `clone` with thread flags, futex incl. requeue, clear-tid wake on thread exit |
| A5 | the `signals` fixture: a guest fault reaches a guest `SIGSEGV` handler and `rt_sigreturn` resumes; `tgkill` interrupts a blocked `futex` wait with `EINTR` | synchronous signal frames, delivery at safe points. ART's implicit null checks (B) rest on this |

**Out of A:**
- `fork`, `vfork`, `execve`: `ENOSYS`, recorded by name. Neither ART nor Roblox's main path
  needs them. Crashpad's `libtrampoline` exec is D's.
- Binder, zygote, ART: B and C.
- Guest `PROT_WRITE|PROT_EXEC`: refused as today. ART's JIT (dual mapping through `memfd`) is B's.
- vDSO: bionic falls back to real syscalls without `AT_SYSINFO_EHDR`. A later performance item.
- `mremap`: only `MREMAP_MAYMOVE`, by copy. `brk` always fails, so bionic uses `mmap`.
- Hosts: Windows first. Linux x86-64 and macOS arm64 after A5; the 16 KiB-page Apple host's
  sub-page `mprotect` is its own item there.

**The sysroot.**
- `tools/make_sysroot.py`, run once on the Linux machine (`simg2img`, `fsck.erofs --extract`,
  `deapexer`), turns the pinned image into a plain tree: `/system`, `/system_ext`, `/product`,
  and every `/apex/<name>` flattened (`com.android.runtime` has bionic and `linker64`,
  `com.android.art` has ART).
- `sysroot.manifest` pins every file by sha256. A run refuses a tree that differs.
- Mounted read-only into each instance and shared by all of them. `/data`, `/dev`, `/proc` and
  `/tmp` are per instance.

## 2. Components

**A new crate, `omni-linux`**, depending on `omni-platform`, `omni-mem`, `omni-elf`, `omni-cpu`.
It sits beside `omni-android` and does not depend on it.

| Module | Job |
|---|---|
| `syscall` | Dense table indexed by the arm64 (asm-generic) syscall number: `fn(&Task, [u64; 6]) -> i64`. Unimplemented: `-ENOSYS`, recorded by name once. |
| `exec` | Maps the program and its `PT_INTERP` without relocating either. Builds the kernel's initial stack: argc, argv, envp, auxv (`AT_PHDR/PHENT/PHNUM`, `AT_BASE`, `AT_ENTRY`, `AT_PAGESZ`, `AT_RANDOM`, `AT_HWCAP`/`AT_HWCAP2` under D26, `AT_EXECFN`, `AT_PLATFORM`, `AT_SECURE`=0, `AT_UID`..`AT_EGID`). |
| `mm` | Linux `mmap` semantics on `GuestSpace`: `MAP_FIXED` replaces, `MAP_FIXED_NOREPLACE`, private writable file mappings (copy on write), executable file mappings, partial `munmap`, `mprotect`, `madvise`, `msync`, `mremap` by copy. Sysroot text is mapped as shared file views so instances share it, as `libroblox.so`'s text is today. |
| `task` | Tasks are guest threads. `clone` (thread flags), `set_tid_address`, `exit`, `exit_group`, `gettid`/`getpid`/`getppid`, futex (`WAIT`, `WAKE`, `*_BITSET`, `REQUEUE`, `CMP_REQUEUE`, `WAKE_OP`; PI refused by name), `sched_*`, a `prctl` subset (`PR_SET_NAME`, `PR_GET_NAME`, `PR_SET_VMA` accepted and ignored). |
| `signal` | Per-task mask and pending set, per-process dispositions. `rt_sigaction`, `rt_sigprocmask`, `rt_sigpending`, `sigaltstack`, `rt_sigreturn`, `tgkill`, `tkill`, `kill` of self. An arm64 `rt_sigframe` laid out exactly as the kernel's. |
| `fd` | File and socket syscalls over `omni_platform::fs` and `net`: `openat` with any dirfd, `close`, `dup`/`dup3`, `fcntl`, `read`/`write`/`readv`/`writev`/`pread64`/`pwrite64`, `lseek`, `getdents64`, `readlinkat`, `newfstatat`/`fstat`/`statx`, `faccessat`, `mkdirat`, `unlinkat`, `renameat2`, `symlinkat`, `chdir`/`fchdir`/`getcwd`, `pipe2`, `eventfd2`, `epoll_create1`/`ctl`/`pwait`, `ppoll`, `pselect6`, `timerfd_*`, the socket family, `ioctl` (`TCGETS` answers `ENOTTY`; others refused by name). |
| `vfs` | The mount table and path resolution (symlinks, `..`, dirfds). The sysroot read-only at `/system`, `/system_ext`, `/product`, `/apex/*`; writable per-instance `/data`, `/tmp`. Synthesized `/proc` (`self/maps`, `self/exe`, `self/fd/*`, `self/status`, `self/cmdline`, `self/task`, `cpuinfo`, `meminfo`, `stat`), `/dev` (`null`, `zero`, `random`, `urandom`, `__properties__/*`), `/sys` (`devices/system/cpu/{possible,present,online}`). |
| `props` | Writes bionic's real property files (`property_info` trie, one `prop_area` per context) from the image's `build.prop` files plus a small omnidroid overlay (`ro.hardware`, `ro.product.cpu.abi`, ...), so the real `libc` maps and reads them unmodified. `setprop` (the `property_service` socket) is refused and recorded. |
| `sys` | `clock_gettime`, `clock_getres`, `clock_nanosleep`, `nanosleep`, `gettimeofday`, `getrandom`, `uname` (Linux 6.1, aarch64), `sysinfo`, `prlimit64`, `getrusage`, the uid/gid calls (a fixed app uid, 10000 + n). |

**Changes to existing crates**, all additive:
- `omni-cpu`: an `SVC #0` is dispatched **inside the run loop** through a handler registered on
  the context, like thunk slots (~30 ns rather than a ~90 ns run-loop exit); only signal
  delivery exits. `GuestThreadConfig` gains guest-managed TLS: real bionic writes `TPIDR_EL0`
  itself, so no backend TLS block is required.
- `omni-mem`: the `MAP_FIXED`-replace, private-writable and executable file-view operations the
  ELF loader already performs internally become public; the reservation's size and placement
  become configurable (B needs low addresses).
- `omni-elf`: a map-only load of an `ET_DYN` object and reading `PT_INTERP`.
- `omni-platform::fs`: `dup`, raw directory reads, `readlink`/`symlink`, a per-instance cwd.

**Running and observing.**
- `omnidroid linux-run --sysroot <dir> -- /system/bin/toybox echo hello`.
- `OMNI_SYSCALL_TRACE=1`: one strace-style line per syscall (`tid name(args) = result`).
- An exit report: exit code or terminating signal, the `ENOSYS` set, refused flag combinations.

## 3. Data flow

**Process start.** `Process::spawn(sysroot, argv, envp)`:
1. Build the instance's `vfs` and write its property files.
2. `exec` maps the program, reads its `PT_INTERP`, maps `linker64`.
3. Map an 8 MiB lazily committed main stack with a guard page; lay out the initial stack.
4. Create the main task: `pc` = `linker64`'s entry, `sp` = the initial stack, other registers 0.
5. Run. `linker64` relocates itself and the program, loads `libc.so` and the rest through
   `openat` + `mmap`, runs constructors, jumps to the program's entry.

**A syscall.** Guest `SVC #0` → dynarmic's `CallSVC` → the context's handler →
`syscall::dispatch(task, x8, x0..x5)` → handler → result in `x0`, or `-errno`. If the task now has
a deliverable signal, the run loop is asked to exit; otherwise execution continues at `pc+4`
without leaving the loop. Blocking calls (futex wait, `epoll_pwait`, pipe and socket reads,
sleeps) block the host thread inside the handler, as blocking thunks do today, and every such
wait also wakes on the task's interrupt, so a signal ends it with `EINTR`. With `SA_RESTART` and
a restartable call, the kernel's rule applies: `pc -= 4`, `x0` = the original `x0`, before the
signal frame is built.

**Thread start.** `clone(flags, stack, ptid, tls, ctid)`:
- The flag set must be the thread set bionic's `pthread_create` uses (`CLONE_VM|FS|FILES|SIGHAND|
  THREAD|SYSVSEM|SETTLS|PARENT_SETTID|CHILD_CLEARTID`); anything else is `-ENOSYS` recorded by
  name (a fork).
- A new tid; `*ptid` written; the child's context copies the parent's registers with `x0` = 0,
  `sp` = `stack`, `TPIDR_EL0` = `tls`; a host thread runs it; the parent gets the tid.

**Thread and process exit.** `exit(code)`: if a clear-tid address is set, write 0 there and wake
one futex waiter; the task ends. `exit_group(code)`: every task is halted at its next check (the
D33/D35 budget and halt path), then the process reports `code`.

**Fault to signal.** A memory fault the demand pager does not claim becomes `SIGSEGV`
(`SEGV_MAPERR` or `SEGV_ACCERR`, `si_addr` = the address); an undefined instruction becomes
`SIGILL`; `BRK` becomes `SIGTRAP`. If the guest has a handler and the signal is not blocked:
- build the frame on the alternate stack if one applies, else below `sp` (red zone kept);
- `pc` = handler, `x0` = signo, `x1` = `&siginfo`, `x2` = `&ucontext`,
  `x30` = `sa_restorer` (bionic always sets `SA_RESTORER`; its `__restore_rt` calls `rt_sigreturn`);
- `rt_sigreturn` restores registers, FP/SIMD state and the mask from the frame.

If the action is default-terminate, the process ends with a report: the signal, the registers,
and `pc` labelled from `/proc/self/maps`.

## 4. Errors

- **The kernel contract.** Every handler answers as Linux would, `-errno` included. Guest input
  never panics a handler; guest pointers go through `omni-mem`'s checked access and fail as
  `EFAULT`.
- **Refusals are named.** An unimplemented syscall is `-ENOSYS`, recorded with its name and the
  first caller's `pc` and `lr` labelled from the maps. An unsupported flag combination is
  `-EINVAL` (or `-ENOSYS` for fork-shaped `clone`), recorded as e.g. `clone: flags 0x11 (fork)`.
  The exit report lists both sets. This is the project's "refuse by name" rule at the kernel
  boundary.
- **Host invariants** (a Rust bug, not guest input) panic and end the instance loudly.

## 5. Testing

- **Unit tests per module**, using the existing hand-assembled A64 harness
  (`tests/harness/a64.rs`): `mmap` semantics (fixed-replace over a partial range, private file
  COW, partial `munmap`), futex requeue and wake-op, `rt_sigframe` field offsets against the
  uapi layout, initial-stack layout and auxv order, property-area bytes against bionic's format.
- **Gate tests A1-A5** in `crates/omni-linux/tests/`, against the pinned sysroot. They skip with
  a named message when it is absent, as the APK gates do.
- **Fixtures for A4 and A5.** The image has no thread or signal test program, so two small C
  programs (`tests/fixtures/threads.c`, `signals.c`) are built with NDK r28c for
  `aarch64-linux-android35` by `tools/build_fixtures.py`. The binaries are committed beside
  their sources, pinned by sha256. This adds the NDK for fixtures only; the runtime stays free
  of it.
- **Mutation rows.** `tools/mutate.py` gains `linux-` rows for the load-bearing semantics
  (`MAP_FIXED` replace, clear-tid wake, `SA_RESTART` rewind, frame layout).
- **Performance guard.** The in-loop syscall round trip is measured on Windows (target ≤ 40 ns,
  against the thunk's 26.7-31.0 ns) and recorded in STATUS.
- **Regression.** The full workspace suite passes. The Roblox path is unchanged, checked by the
  owner's routine at the end of A: Windows, then the Mac and the Linux machine updated from
  `unified`, each running `omnidroid --cookie Desktop/cookies/<its file> --place 8737899170`,
  with a screenshot once the game has loaded.

## 6. Risks carried forward

| Risk | Where it bites | Plan |
|---|---|---|
| ART's heap and boot image want addresses below 4 GiB (compressed references); identity mapping makes that a host requirement | B | A makes the reservation's placement configurable; B proves low placement on each host first |
| Real bionic's `memcpy`, `strlen` and the like run translated where today they are host Rust | D's frame rate | measure in D; if needed, host-accelerated overrides by address, as an optimization, never for correctness |
| `linker64` maps segments at 4 KiB file offsets; Windows file views want 64 KiB | A1 | `mm` uses the loader's existing sub-granule technique; where a view is impossible it copies (private), losing sharing, never correctness |
| Syscall surface growth | A-D | the `ENOSYS` report makes every gap a named, one-time fix |
