"""Mutation testing across the workspace, table-driven.

    python tools/mutate.py               # from the repository root
    python tools/mutate.py --only mem    # one prefix
    python tools/mutate.py --list

Global Constraint 12: a test that does not fail when the logic it covers is reverted is not
evidence. `crates/omni-elf/tools/mutate_loader.py` made that checkable for the loader; this is the
generalisation the whole-branch review asked for — it takes a table of (file, old, new, command)
rather than being loader-shaped, so a new fix anywhere in the workspace costs one table row.

Each mutation is applied on its own, the named test command is run, the result is recorded, and the
file is restored via `try`/`finally` -- including on a crash, but **not** if the interpreter is
killed. That gap is real and has been hit: a run killed mid-row left `arena.rs` carrying its
mutation, and `git status` showed only "modified", which is what the file looks like during ordinary
work. The pre-flight below is what turns that from a silent corruption into a one-second refusal on
the next run, because a stale tree makes some pattern fail to match. If a run is ever killed, check
`git diff` before trusting the tree.

A mutation that does not compile, does not match its pattern, or is caught by nothing is reported as
`MISS`, never as a pass: a mutation nothing notices means the fix has no test behind it.

Two directions, and the second is the point:

* **A** reverts a fix. Something must fail.
* **B** over-corrects — bounds something that should not be bounded, commits eagerly where the
  design commits lazily. These read as correct and destroy a measured property, and they are the
  direction that is normally missing.

**Do not stage or commit while this is running, and do not run two copies of it.** It mutates files
in the working tree in place, so `git add` during a run can capture a mutation, and the commit then
looks like ordinary work with every test passing — the mutation is restored before the suite next
runs. That happened once, in M3 task 1: `XReg::new`'s bound came back as `>` instead of `>=`, which
admits `X31`, and it was found by reading the commit rather than by running anything. The pre-flight
below catches a *killed* run on the next invocation; it cannot see a concurrent one.

`--no-fail-fast` is not optional: without it `cargo test` stops after the first failing binary and
silently attributes every mutation to whichever binary happened to run first.
"""

import argparse
import subprocess
import sys
import time

PLAT = "crates/omni-platform/src/vm/mod.rs"
SPACE = "crates/omni-mem/src/space.rs"
ARENA = "crates/omni-mem/src/arena.rs"
BUDGET = "crates/omni-mem/src/budget.rs"
LOADER = "crates/omni-elf/src/loader/mod.rs"
ZIP = "crates/omni-apk/src/zip.rs"
CPU_CONTEXT = "crates/omni-cpu/src/context.rs"
CPU_REGS = "crates/omni-cpu/src/regs.rs"
CPU_FASTMEM = "crates/omni-cpu/src/fastmem.rs"
CPU_RUN = "crates/omni-cpu/src/run.rs"
CPU_TLS = "crates/omni-cpu/src/tls.rs"
CPU_CLOCK = "crates/omni-cpu/src/clock.rs"
CPU_CALLBACKS = "crates/omni-cpu/src/dynarmic/callbacks.rs"
CPU_DYN = "crates/omni-cpu/src/dynarmic/mod.rs"
PAGER = "crates/omni-mem/src/pager.rs"
ACCESS = "crates/omni-mem/src/access.rs"
BIONIC_ERRNO = "crates/omni-bionic/src/errno.rs"
BIONIC_LAYOUTS = "crates/omni-bionic/src/layouts.rs"
BIONIC_STRING = "crates/omni-bionic/src/string.rs"
BIONIC_WIDE = "crates/omni-bionic/src/wide.rs"
BIONIC_SEM = "crates/omni-bionic/src/sem.rs"
BIONIC_MUTEX = "crates/omni-bionic/src/mutex.rs"
BIONIC_NUMERICS = "crates/omni-bionic/src/numerics.rs"
BIONIC_RWLOCK = "crates/omni-bionic/src/rwlock.rs"
BIONIC_COND = "crates/omni-bionic/src/cond.rs"
# M6: `sched_get_priority_max`/`_min`, which the engine sizes a real-time band against.
BIONIC_METADATA = "crates/omni-bionic/src/metadata.rs"
BIONIC_PRINTF = "crates/omni-bionic/src/printf.rs"
# M6: the guest frame-pointer walk, which reads guest-supplied pointers (D6).
BIONIC_UNWIND = "crates/omni-bionic/src/unwind.rs"
FAULT = "crates/omni-platform/src/fault/windows.rs"
EH_FRAME = "crates/omni-elf/src/eh_frame.rs"
LEAF = "crates/omni-elf/src/leaf.rs"
ABI = "crates/omni-android/src/abi.rs"
VARARGS = "crates/omni-android/src/varargs.rs"
ANDROID_MEM = "crates/omni-android/src/mem.rs"
REGION = "crates/omni-android/src/region.rs"
BOUNDARY = "crates/omni-android/src/boundary.rs"
# The bionic adapter: `omni-bionic`'s functions bound onto the boundary.
ADAPTER_VIEW = "crates/omni-android/src/bionic/view.rs"
ADAPTER_MOD = "crates/omni-android/src/bionic/mod.rs"
ADAPTER_HANDLERS = "crates/omni-android/src/bionic/handlers.rs"
ADAPTER_FORMAT = "crates/omni-android/src/bionic/format.rs"
ADAPTER_DATA = "crates/omni-android/src/bionic/data.rs"
ADAPTER_DL = "crates/omni-android/src/bionic/dl.rs"
ADAPTER_GUESTMEM = "crates/omni-android/src/bionic/guestmem.rs"
# M3 task 3 phase 3a: the OS surface. `omni-platform` grows past `vm` and `fault`, and the adapter
# grows the twenty-three guest symbols over it.
PLAT_CLOCK = "crates/omni-platform/src/clock.rs"
PLAT_LOG = "crates/omni-platform/src/log.rs"
PLAT_PROCESS = "crates/omni-platform/src/process/windows.rs"
PLAT_PROCESS_MOD = "crates/omni-platform/src/process/mod.rs"
BIONIC_TIME = "crates/omni-bionic/src/time.rs"
ADAPTER_CLOCKS = "crates/omni-android/src/bionic/clocks.rs"
ADAPTER_PROCENV = "crates/omni-android/src/bionic/procenv.rs"
ADAPTER_SIGNALS = "crates/omni-android/src/bionic/signals.rs"
BIONIC_STDIO = "crates/omni-bionic/src/stdio.rs"
LIBM = "crates/omni-bionic/src/libm.rs"
BIONIC_LOCALE = "crates/omni-bionic/src/locale.rs"
BIONIC_WIDE = "crates/omni-bionic/src/wide.rs"
ADAPTER_LOGGING = "crates/omni-android/src/bionic/logging.rs"
# M3 task 3 phase 3b: files and directories. `omni-platform` gains a ROOTED filesystem, the
# `FILE *` layer lands in `omni-bionic` over a trait, and the adapter binds the 29 file-io symbols.
PLAT_FS = "crates/omni-platform/src/fs/mod.rs"
PLAT_FS_PATH = "crates/omni-platform/src/fs/path.rs"
# M5: the pipe. An in-process byte queue with two ends, and the first descriptor kind whose
# readiness depends on another descriptor.
PLAT_FS_PIPE = "crates/omni-platform/src/fs/pipe.rs"
# M6: the eventfd. The second kind whose readiness is state rather than a constant, and the first
# whose *read* changes what the next read answers.
PLAT_FS_EVENTFD = "crates/omni-platform/src/fs/eventfd.rs"
# M5: the NDK surface. `ALooper` is the first of the four families.
NDK_LOOPER = "crates/omni-android/src/ndk/looper.rs"
# M6: `ANativeWindow` is the fourth. Five symbols, because that is what `libroblox.so` imports --
# §4.4's "ANativeWindow (9)" is the count across the whole APK.
NDK_WINDOW = "crates/omni-android/src/ndk/window.rs"
NDK_MOD = "crates/omni-android/src/ndk/mod.rs"
PLAT_FS_WINDOWS = "crates/omni-platform/src/fs/windows.rs"
BIONIC_STDIO = "crates/omni-bionic/src/stdio.rs"
ADAPTER_FILES = "crates/omni-android/src/bionic/files.rs"
ADAPTER_STDIO = "crates/omni-android/src/bionic/stdio.rs"

# Phase 3d/3e: the network group and the six nothing else claimed. `omni-platform` gained one
# primitive for this phase (process CPU time) and **no socket seam at all** -- `poll` and `select`
# answered over the descriptor table `fs` already had.
#
# **That last sentence stopped being true in M6.** D30 withdrew Global Constraint 8, `omni-platform`
# grew a `net` module, and a socket is a descriptor in the same table -- so `poll` and `select` now
# make a real host readiness call. The `sock-` rows below are the marshalling that came with it: a
# `struct addrinfo` list in GUEST memory, its `sockaddr`s, and the bounded slab they live in.
BIONIC_NET = "crates/omni-bionic/src/net.rs"
ADAPTER_NET = "crates/omni-android/src/bionic/net.rs"
# M6: the guest's `struct addrinfo` layout and the slab `getaddrinfo` builds a list in. Its own
# file because every byte in it is read by the guest, and because the field ORDER is the thing
# that is dangerous to get wrong -- bionic puts `ai_canonname` before `ai_addr` and glibc reverses
# them, with the same `sizeof` either way.
ADAPTER_ADDRINFO = "crates/omni-android/src/bionic/addrinfo.rs"

# M6, the socket-configuration follow-on: the keep-alive TIMING options and `getsockname`.
# `omni-platform`'s net backend is mutated for the FIRST time here, and it is the one file in
# this workspace that knows the host's socket-option numbers. The `sockcfg-` rows are about one
# failure mode and it is the worst kind this table has: an option number mapped to the wrong host
# constant SUCCEEDS. `setsockopt` returns zero, the socket keeps working, and the only difference
# is a keep-alive that fires at the wrong time on a connection nobody is watching. Linux numbers
# these 4, 5, 6; Windows numbers the same three 3, 17, 16 and puts TCP_MAXRT on 5.
PLATFORM_NET_WINDOWS = "crates/omni-platform/src/net/windows.rs"
PLATFORM_NET_MOD = "crates/omni-platform/src/net/mod.rs"
# M6: the opt-in socket record. Its two failure modes are both silent -- a recorder that keeps
# capturing after it was turned off is a credential leak that looks like a working recorder, and a
# budget that is not enforced is an unbounded buffer that only shows up on a long run.
PLATFORM_NET_RECORD = "crates/omni-platform/src/net/record.rs"

# Phase 3c: threads and signals.
BIONIC_SIGNAL = "crates/omni-bionic/src/signal.rs"
BIONIC_LAYOUTS = "crates/omni-bionic/src/layouts.rs"
ADAPTER_SIGNALS = "crates/omni-android/src/bionic/signals.rs"
ADAPTER_THREADS = "crates/omni-android/src/bionic/threads.rs"
ADAPTER_RUNTIME = "crates/omni-android/src/bionic/runtime.rs"
# M4: JNI without a JVM. The JNI modules, and the two bionic files M4's gate corrected.
JNI_ENV = "crates/omni-android/src/jni/env.rs"
JNI_REFS = "crates/omni-android/src/jni/refs.rs"
JNI_CLASSES = "crates/omni-android/src/jni/classes.rs"
# M6 row 21: the scripted downcall table. The one string in it that decided whether the engine
# could ever get its flags.
JNI_SCRIPT = "crates/omni-android/src/jni/script.rs"
# The instance: where a Java statement's store into a static lands (`Jni::put_static_object`).
JNI_MOD = "crates/omni-android/src/jni/mod.rs"
# M6: the startup gate itself. Two `jmid-` rows are anchored in it.
GATE_ACTIVITY_FILE = "crates/omni-android/tests/gameactivity.rs"
JNI_VALUES = "crates/omni-android/src/jni/values.rs"
JNI_POOL = "crates/omni-android/src/jni/pool.rs"
JNI_SLOTS = "crates/omni-android/src/jni/slots.rs"
# §8 row 26: the Java side's touch listener, `vk.e.onTouch`, and the seam that feeds it the host
# window's pointer.
JNI_INPUT = "crates/omni-android/src/jni/input.rs"
# And `vk.g`, the hardware-key path, with the window seam's physical-key decode it depends on.
JNI_KEYS = "crates/omni-android/src/jni/keys.rs"
PLAT_WINDOW_WINDOWS = "crates/omni-platform/src/window/windows.rs"
# `/proc/meminfo` and `/proc/self/statm`: the adapter's two generated files, the seam's generated
# file kind they are served through, and the process-memory snapshot `statm` is read from.
ADAPTER_PROCFS = "crates/omni-android/src/bionic/procfs.rs"
PLAT_VM_WINDOWS = "crates/omni-platform/src/vm/windows.rs"


# Commands, kept narrow so the whole run stays under a few minutes.
MEM = ["cargo", "test", "-p", "omni-mem", "--no-fail-fast"]
# `omni-bionic` has no dependencies at all, so it builds in seconds. The targets are named rather
# than taking the whole package because `tests/stress.rs` runs 8 threads x 12,500 rounds and is
# minutes in a debug build, while asserting nothing these rows touch -- the same reasoning as
# `ANDROID` above. `sem_wakeup` IS named: the waiter-flag row is what it exists for.
BIONIC = [
    "cargo", "test", "-p", "omni-bionic", "--lib",
    "--test", "string_tests", "--test", "wide_tests", "--test", "numerics_tests",
    "--test", "mem_tests", "--test", "sem_wakeup",
    # `printf_tests` is named because the field-width and output caps live there, and they are
    # the only bound in this crate that a guest-chosen value can push against.
    "--test", "printf_tests",
    # M5 added two integration targets and **every row that needs one would have reported a false
    # MISS without them** -- which is entry 8's own lesson, arriving as a prediction rather than
    # as a surprise this time: both agents that wrote these rows said so in their reports before
    # anything was run.
    "--test", "strftime_tests", "--test", "stdio_modes_tests",
    # M6 added a third for the same reason, predicted the same way: `errno-A1`, `errno-A3`,
    # `errno-B1`, `errno-B2` and `errno-B4` are caught HERE AND NOWHERE ELSE. Leaving it out would
    # have turned five rows into MISSes that read as missing tests rather than a missing target.
    "--test", "stdio_errno_tests",
    "--no-fail-fast",
]
CPU = ["cargo", "test", "-p", "omni-cpu", "--no-fail-fast"]
PLATFORM = ["cargo", "test", "-p", "omni-platform", "--no-fail-fast"]
# The scanf engine and its adapter: the engine's own cases, and a real variadic guest call.
SCANF = ["cargo", "test", "-p", "omni-bionic", "--test", "scanf_tests", "--no-fail-fast"]
# The demand pager is policy in `omni-mem` driven by execution in `omni-cpu`, so a mutation of it
# has to run both: its unit tests live with the code and its behavioural tests live with the guest
# that provokes the faults. A row scoped to one of the two reported a MISS that was a gap in the
# harness rather than in the tests, which is worth leaving written down.
MEM_AND_CPU = ["cargo", "test", "-p", "omni-mem", "-p", "omni-cpu", "--no-fail-fast"]
ELF = ["cargo", "test", "-p", "omni-elf", "--no-fail-fast"]
APK = ["cargo", "test", "-p", "omni-apk", "--no-fail-fast"]
# The leaf scan decodes 245,117 function bodies, which is minutes in a debug build and a quarter of
# a second in release. Its own command rather than widening `ELF`, so the rest of the ELF rows keep
# running against the build everything else uses.
ELF_SCAN = [
    "cargo", "test", "-p", "omni-elf", "--release", "--lib", "--test", "eh_frame_golden",
    "--no-fail-fast",
]
# The commit-charge figures are only meaningful in a release build.
ELF_RELEASE = [
    "cargo", "test", "-p", "omni-elf", "--release", "--test", "loader_commit", "--no-fail-fast",
]

# The thunk boundary. Scoped to the three fast targets rather than the whole package: the
# `libroblox` target loads 109 MB and applies 568,806 relocations, and it asserts about the
# *loader's* binding rather than about the marshalling these rows mutate, so including it would
# multiply every row's cost by that load for no extra detection.
ANDROID = [
    "cargo", "test", "-p", "omni-android", "--lib", "--test", "roundtrip", "--test", "hostile",
    # The bionic adapter's own target. Added when the adapter was written: every row below that
    # names an `ADAPTER_*` file is detected here and nowhere else, so leaving it out would have
    # turned each of them into a MISS that looked like a missing test rather than a missing
    # target.
    "--test", "bionic",
    # The NDK surface's own target, added in M5 for exactly the same reason: every `NDK_*` row is
    # detected here and nowhere else.
    "--test", "ndk",
    "--no-fail-fast",
]

# The adapter's **library** targets only, with no guest in sight. One row needs this and says why:
# removing the sleep cap makes the end-to-end test sleep for the `i64::MAX` seconds it asked for,
# which HANGS rather than fails -- the failure mode this module's docstring already records from M3
# task 2. Its detector is the unit test on `clocks::capped`, which is why that predicate is a
# function rather than an inline comparison.
ANDROID_LIB = ["cargo", "test", "-p", "omni-android", "--lib", "--no-fail-fast"]

# The Vulkan layer's own targets: every `vulkan/` row is detected by these and nothing else. Added
# with the first rows for it (M6, the engine's device bring-up).
VULKAN = [
    "cargo", "test", "-p", "omni-android", "--test", "vulkan_device", "--test", "vulkan_instance",
    "--test", "vulkan_loader", "--test", "vulkan_present", "--no-fail-fast",
]
# `libm_tests` alone: the math primitives' own target, which `BIONIC` does not name.
BIONIC_LIBM = ["cargo", "test", "-p", "omni-bionic", "--test", "libm_tests", "--no-fail-fast"]

# The startup gate, FILTERED to one test by name. `ANDROID` deliberately does not name
# `--test gameactivity`: that target runs the real APK end to end and takes ~50 s, which is
# the same reasoning that keeps `tests/stress.rs` out of `BIONIC`. The two rows that need it
# are detected by `the_activity_class_answers_every_member_row_23_looks_up_on_it`, which needs
# neither the APK nor a guest run -- so this costs a build and not a run.
GATE_ACTIVITY = ["cargo", "test", "-p", "omni-android", "--release", "--test", "gameactivity",
                 "--no-fail-fast", "the_activity_class"]

# The same target, filtered to the test that reads `bh.x0.M` out of the APK's dex. It opens the
# APK and reads four dex files -- no guest, no CPU, no network -- so like `GATE_ACTIVITY` it costs
# a build and not a run. Separate from `GATE_ACTIVITY` because that one is filtered by name to a
# different test, and a row pointed at the wrong filter reports MISS rather than being wrong.
GATE_APPNAME = ["cargo", "test", "-p", "omni-android", "--release", "--test", "gameactivity",
                "--no-fail-fast", "the_application_name"]

# `libaaudio.so`: the module's unit tests (in the lib target) and `tests/aaudio.rs`, which drives it
# from guest code through `dlopen`/`dlsym` and a guest data callback on a guest thread, into a
# recording device. No APK and no audio hardware, so every row costs a build and not a run.
AAUDIO = ["cargo", "test", "-p", "omni-android", "--lib", "--test", "aaudio", "--no-fail-fast"]

# The Java side's web view (`jni::webview`, and its two answers in `jni::env`): the module's unit
# tests alone, filtered by path. No APK, no guest and no browser -- the decisions are a state
# machine and the answers are reached through `env::evaluate` -- so every row costs a build.
WEBVIEW = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "jni::webview"]

# The condattr bindings, from guest code: `tests/bionic.rs` filtered to the one test. No APK.
BIONIC_CONDATTR = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                   "condattr"]
# The scheduling-parameter bindings, from guest code: `tests/bionic.rs` filtered by name. No APK.
BIONIC_SCHED = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                "sched"]
BIONIC_HOSTNAME = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                   "gethostname"]
JNI_THEME = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "system_theme"]
BIONIC_GAI = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
              "getaddrinfo"]
FUTEX_RUNTIME = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "bionic::runtime"]
BIONIC_MUTEX_LIB = ["cargo", "test", "-p", "omni-bionic", "--release", "--lib", "--no-fail-fast", "mutex"]
BIONIC_CLOEXEC = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                  "close_on_exec"]
BIONIC_LINGER = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                 "linger"]
PLAT_LINGER = ["cargo", "test", "-p", "omni-platform", "--release", "--test", "net_loopback", "--no-fail-fast",
               "linger"]
BIONIC_PMTU = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
               "path_mtu"]
PLAT_PMTU = ["cargo", "test", "-p", "omni-platform", "--release", "--test", "net_loopback", "--no-fail-fast",
             "path_mtu"]
BIONIC_AUXV = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
               "getauxval"]
LIBM_SINCOS = ["cargo", "test", "-p", "omni-bionic", "--release", "--test", "libm_tests", "--no-fail-fast",
               "sincos_"]
# A loaded world's capacity: the thread and stream tables from guest-facing tests, the JNI env
# table and the arena's stated granules from the lib's unit tests. No APK.
CAPACITY_BIONIC = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                   "a_loaded_worlds"]
CAPACITY_JNI = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast",
                "a_loaded_worlds"]
CAPACITY_ARENA = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast",
                  "the_arena_spans"]
# `vkCmdCopyImageToBuffer`: the guest-side handler, through the recording host double.
VULKAN_READBACK = ["cargo", "test", "-p", "omni-android", "--release", "--test", "vulkan_present",
                   "--no-fail-fast", "an_image_copy_carries"]
# Inbound sockets (`listen`, `accept`) and the host's interface addresses: the seam and the policy
# from their own tests, the guest's calls from `tests/bionic.rs`, the Java body from the lib.
INBOUND_PLAT = ["cargo", "test", "-p", "omni-platform", "--release", "--lib", "--test", "net_loopback",
                "--no-fail-fast"]
INBOUND_BIONIC = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
                  "listen_and_accept"]
INBOUND_JNI = ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "public_ipv4"]

# `__vsprintf_chk` (a game world, 2026-09-23): its two tests.
VSPRINTF = ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast",
            "vsprintf_chk"]

# The same target, filtered to the test of the exit records the gate keeps in a kept root. Files in
# a temporary directory -- no APK, no guest -- so it costs a build and not a run.
GATE_EXITS = ["cargo", "test", "-p", "omni-android", "--release", "--test", "gameactivity",
              "--no-fail-fast", "exit_records"]

# The touch seam: `jni::input`'s unit tests (in the lib target) and `tests/input.rs`, which calls a
# hand-assembled stand-in for the native through real translated code and reads back the registers
# it was called with. No APK and no engine, so every row costs a build and not a run.
INPUT = ["cargo", "test", "-p", "omni-android", "--lib", "--test", "input", "--no-fail-fast"]

# `omni-platform`'s unit tests alone. `PLATFORM` names the whole package, whose live network and
# window targets need a gate and a network; the window backend's key decode is tested in the lib.
PLATFORM_LIB = ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast"]

# The process-memory snapshot's own target: every field is pinned there by making its quantity
# move, and nowhere else measures the host's counters. Its own binary because commit charge is
# per-process (see that file's header).
PROCESS_MEMORY = [
    "cargo", "test", "-p", "omni-platform", "--test", "vm_commit_charge", "--no-fail-fast",
]
# The adapter's `/proc` files: `procfs`' unit tests (the exact bytes) and the guest-side tests in
# `bionic` (the engine's own `__open_2` + `pread` + `sscanf`, and `sysinfo` in the same run).
PROCFS = ["cargo", "test", "-p", "omni-android", "--lib", "--test", "bionic", "--no-fail-fast"]

# M6 groundwork: runtime texture transcoding (`omni-texture`). Zero dependencies and `#![no_std]`,
# so its command builds in about a second.
TEXTURE_ETC1 = "crates/omni-texture/src/etc1.rs"
TEXTURE_LIB = "crates/omni-texture/src/lib.rs"
TEXTURE_FORMAT = "crates/omni-texture/src/format.rs"
# `tests/exhaustive.rs` is deliberately NOT named here. MEASURED: 27.4 s in a debug build against
# under a second for the rest of the crate, and every row below has a named detector in one of the
# three targets that ARE named -- the same reasoning, and the same precedent, as `BIONIC` leaving
# out `tests/stress.rs`. `real_assets` is named because two rows are only caught there.
TEXTURE = [
    "cargo", "test", "-p", "omni-texture", "--lib",
    "--test", "spec_vectors", "--test", "hostile", "--test", "real_assets",
    "--no-fail-fast",
]

# (id, direction, description, file, old, new, command)
MUTATIONS = [
    # ---- the commit ceiling: the Critical -------------------------------------------------------
    ("mem-A1", "A", "per-request commit ceiling removed", SPACE,
     """        if len > self.max_commit_request {""",
     """        if false && len > self.max_commit_request {""",
     MEM),

    ("mem-A2", "A", "total commit ceiling removed", SPACE,
     """        if would_total > self.max_committed {""",
     """        if false && would_total > self.max_committed {""",
     MEM),

    ("mem-A3", "A", "the ceiling checked after the commit instead of before", SPACE,
     """            self.check_commit_allowed(operation, from, to - from)?;

            self.make_exact_placeholder(operation, from, to - from, false)?;""",
     """            self.make_exact_placeholder(operation, from, to - from, false)?;""",
     MEM),

    ("mem-A4", "A", "committed total never comes back down on unmap", SPACE,
     """                    self.committed -= entry.len;
                    self.map.free_range(start, entry.len);
                    position = entry_end;""",
     """                    self.map.free_range(start, entry.len);
                    position = entry_end;""",
     MEM),

    ("mem-A5", "A", "committed total never comes back down on reclaim_idle", SPACE,
     """            entry.os = OsState::Placeholder;
            self.committed -= len;""",
     """            entry.os = OsState::Placeholder;""",
     MEM),

    ("mem-A6", "A", "a refused eager commit leaves its mapping behind", SPACE,
     """                if let Err(rollback) = inner.unmap_range(OP, address, size) {""",
     """                if let Err(rollback) = (if true { Ok(()) } else { inner.unmap_range(OP, address, size) }) {""",
     MEM),

    # ---- the ceilings must separate the attack from legitimate growth ---------------------------
    # The pair only works if each stays on its own side of the gap, so both sides are mutated. A1-A3
    # above prove the attack is refused; these two prove the legitimate case is not, which is the
    # direction that would let a security fix quietly break the feature.
    ("mem-B2", "B", "the per-request ceiling tightened below a legitimate eager mapping", SPACE,
     """pub const DEFAULT_MAX_COMMIT_REQUEST: usize = 128 * 1024 * 1024;""",
     """pub const DEFAULT_MAX_COMMIT_REQUEST: usize = 32 * 1024 * 1024;""",
     MEM),

    ("mem-B3", "B", "the total ceiling lowered below D10's validated 3 GB of live use", SPACE,
     """pub const DEFAULT_MAX_COMMITTED: usize = 3584 * 1024 * 1024;""",
     """pub const DEFAULT_MAX_COMMITTED: usize = 2048 * 1024 * 1024;""",
     MEM),

    # ---- the ceiling must not bind on the lazy path (direction B) -------------------------------
    ("mem-B1", "B", "lazy commit made eager: a granule becomes the whole mapping", SPACE,
     """                CommitPolicy::Lazy => owner.granule,""",
     """                CommitPolicy::Lazy => owner.mapping_len,""",
     MEM),

    # The first attempt at this mutation made `commit_range`'s lazy granule the whole mapping, which
    # was NOT CAUGHT — correctly, because nothing ever calls `commit_range` on a lazy `.bss` mapping
    # in this library: no relocation targets `.bss`. The mutation has to be where the policy is
    # *chosen*, not where it is applied.
    ("elf-B2", "B", "the .bss policy ignored, so lazy .bss is committed eagerly anyway", LOADER,
     """                    config.bss_commit,""",
     """                    CommitPolicy::Eager,""",
     ELF_RELEASE),

    # ---- arena identity --------------------------------------------------------------------------
    ("mem-A7", "A", "foreign blocks accepted again", ARENA,
     """        if block.arena != self.id {""",
     """        if false && block.arena != self.id {""",
     MEM),

    # Reached through the pure `block_fits_chunk` rather than through `reprotect`: after `check_own`
    # the containment check is unreachable from the public API, which is the point of having both, so
    # the arithmetic is asked directly. The underflow it guards was a release-only defect.
    ("mem-A8", "A", "a block below its chunk no longer refused (the release underflow)", ARENA,
     """    write >= chunk_write""",
     """    write >= chunk_write.saturating_sub(usize::MAX)""",
     MEM),

    ("mem-A9", "A", "an absurd block alignment accepted again", ARENA,
     """        if config.block_alignment > config.chunk_size {""",
     """        if false && config.block_alignment > config.chunk_size {""",
     MEM),

    # ---- the arena's sealed pages, and the budget that instruments what the OS counter cannot see -
    # `CodeArena::write` is a *safe* function that stores through the writable view, and `seal` makes
    # that view PAGE_READONLY. Reverting the check does not make a test fail politely: it kills the
    # test process with an access violation, which is exactly the point — that is what safe code
    # could reach before it existed.
    ("mem-A11", "A", "the sealed-page check removed, so a safe write faults the process", ARENA,
     """        let sealed_path = self.sealed_pages.load(Ordering::Acquire) != 0;""",
     """        let sealed_path = false && self.sealed_pages.load(Ordering::Acquire) != 0;""",
     MEM),

    # The guard held across the store (the review's I1). Previously unpinnable without a racing
    # test, which was rightly refused: a test that has to lose a race to fail corrupts this very
    # table. `debug_assert!(!sealed_path || self.inner.is_locked())` makes it deterministic instead,
    # and the slow path's success-path test is what executes it.
    # `let _ = expr` drops the temporary at the end of *that statement*, while `let _name = expr`
    # holds it to the end of scope. So this one-character edit is the real shape of the bug, and it
    # compiles -- `drop(chunks); None` does not, because both arms would then be `None` and the
    # guard type becomes uninferable.
    ("mem-A14", "A", "the arena guard dropped before the store instead of held across it", ARENA,
     """        let _sealed_guard = if sealed_path {""",
     """        let _ = if sealed_path {""",
     MEM),

    ("mem-A12", "A", "the budget stops adding the arena's invisible commit to the total", BUDGET,
     """        self.process_private + self.arena_mapped as u64""",
     """        self.process_private""",
     MEM),

    ("mem-A13", "A", "the budget reports nothing as invisible to the process counter", BUDGET,
     """    pub fn invisible_to_process_counter(&self) -> usize {
        self.arena_mapped
    }""",
     """    pub fn invisible_to_process_counter(&self) -> usize {
        0
    }""",
     MEM),

    # Direction B for the arena, and the reason chunking exists at all. A pagefile-backed section is
    # charged against the system commit limit when it is *created* (D15), so an arena that maps its
    # whole 256 MiB ceiling up front is correct in every functional sense and thirty-two times more
    # expensive for the 8 MiB of code that was actually emitted.
    ("mem-B4", "B", "the arena maps its whole ceiling up front instead of growing a chunk at a time",
     ARENA,
     """pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;""",
     """pub const DEFAULT_CHUNK_SIZE: usize = 256 * 1024 * 1024;""",
     MEM),

    # Direction B for the sealed-page fix itself: a refusal that goes too far. Every hostile-input
    # test still passes — more of them pass, in fact — and the arena can never patch a block again,
    # which is the one thing a JIT must be able to do on invalidation.
    ("mem-B5", "B", "unseal leaves the pages marked sealed, so no block can ever be patched", ARENA,
     """        let changed = chunk.set_sealed(pages, protection == Protection::Read);""",
     """        let changed = chunk.set_sealed(pages, true);""",
     MEM),

    # ---- the CPU seam ----------------------------------------------------------------------------
    # D13: 1,276 of libroblox.so's 1,282 MRS TPIDR_EL0 instructions load [Xt, #0x28], and the first
    # runs before the first static initializer. A thread that can be created without a thread pointer
    # is a thread that crashes inexplicably later.
    ("cpu-A1", "A", "a guest thread may be created with a null TPIDR_EL0 again", CPU_CONTEXT,
     """        if tpidr_el0 == 0 {""",
     """        if false && tpidr_el0 == 0 {""",
     CPU),

    ("cpu-A2", "A", "X31 becomes a general-purpose register again", CPU_REGS,
     """        if index as usize >= Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "X", index: index as u32, count: 31 });""",
     """        if index as usize > Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "X", index: index as u32, count: 31 });""",
     CPU),

    # Direction B for the thread-pointer check: a stricter rule that is not required by anything.
    # bionic's TLS block is not page-aligned in general, so this refuses legitimate threads while
    # every "hostile input is rejected" assertion keeps passing.
    ("cpu-B1", "B", "the thread pointer is additionally required to be page-aligned", CPU_CONTEXT,
     """        if tpidr_el0 == 0 {""",
     """        if tpidr_el0 == 0 || tpidr_el0 % 4096 != 0 {""",
     CPU),

    # ---- the guest space's other edges -----------------------------------------------------------
    ("mem-A10", "A", "an alignment larger than the space accepted again", SPACE,
     """        if align > self.len {""",
     """        if false && align > self.len {""",
     MEM),

    # ---- the platform seam's descriptor edges ----------------------------------------------------
    ("plat-A1", "A", "zero-length subrange accepted again", PLAT,
     """        check_size("subrange", len)?;""",
     """        if false { check_size("subrange", len)?; }""",
     PLATFORM),

    ("plat-A2", "A", "contains() admits a zero-length range at end()", PLAT,
     """        if len == 0 {
            return false;
        }
        let address = ptr as usize;
        address >= self.base && address < self.end() && len <= self.end() - address""",
     """        let address = ptr as usize;
        address >= self.base && address <= self.end() && len <= self.end() - address""",
     PLATFORM),

    # ---- the loader ------------------------------------------------------------------------------
    ("elf-A1", "A", "parsed bytes and mapped file no longer tied together", LOADER,
     """    if backing.len() != elf.data().len() as u64 {""",
     """    if false && backing.len() != elf.data().len() as u64 {""",
     ELF),

    ("elf-A2", "A", "the plan's anonymous memory is unbounded again", LOADER,
     """    if anonymous > config.max_anonymous_bytes {""",
     """    if false && anonymous > config.max_anonymous_bytes {""",
     ELF),

    # ---- omni-apk --------------------------------------------------------------------------------
    ("apk-A1", "A", "a local header naming a different entry is accepted", ZIP,
     """    if local_name != record.name {""",
     """    if false && local_name != record.name {""",
     APK),

    # ---- D4: identity mapping, and the assertion that is its entire defence -----------------------
    ("cpu-A15", "A", "the D4 startup assertion cannot refuse anything", CPU_FASTMEM,
     """    let refuse = |setting, expected: u64, actual: u64, consequence| {
        Err(CpuError::MisconfiguredMemoryPath { setting, expected, actual, consequence })
    };""",
     """    let refuse = |_setting, _expected: u64, _actual: u64, _consequence| Ok(());""",
     CPU),

    ("cpu-A16", "A", "the fastmem width is no longer required to be 64", CPU_FASTMEM,
     """    if observed.address_bits != 64 {""",
     """    if false && observed.address_bits != 64 {""",
     CPU),

    ("cpu-A17", "A", "a wild guest address may be mirrored into range again", CPU_FASTMEM,
     """    if observed.mirrors_out_of_range {""",
     """    if false && observed.mirrors_out_of_range {""",
     CPU),

    # ---- the run loop: the watchdog and the signed-comparison footgun -----------------------------
    ("cpu-A18", "A", "a budget reaches the backend unclamped, so u64::MAX reads as negative",
     CPU_RUN,
     """    if wanted == 0 {
        1
    } else if wanted > MAX_SLICE_INSTRUCTIONS {
        MAX_SLICE_INSTRUCTIONS
    } else {
        wanted
    }""",
     """    wanted""",
     CPU),

    ("cpu-A19", "A", "a zero budget becomes run-forever instead of one instruction", CPU_RUN,
     """    if wanted == 0 {
        1
    } else if""",
     """    if wanted == 0 {
        0
    } else if""",
     CPU),

    # ---- D13: the bionic thread pointer ----------------------------------------------------------
    ("cpu-A20", "A", "the stack guard is never written into slot 5", CPU_TLS,
     """            ptr.add(TLS_SLOT_STACK_GUARD_OFFSET)
                .cast::<u64>()
                .write_unaligned(self.guard);""",
     """            let _ = TLS_SLOT_STACK_GUARD_OFFSET;""",
     CPU),

    ("cpu-A21", "A", "a recycled TLS block keeps the previous thread's contents", CPU_TLS,
     """            core::ptr::write_bytes(ptr, 0, self.block_bytes);""",
     """            if false { core::ptr::write_bytes(ptr, 0, self.block_bytes); }""",
     CPU),

    ("cpu-A22", "A", "the stack guard may be zero, which equals a zeroed stack slot", CPU_TLS,
     """        let value = hasher.finish();
        if value != 0 {
            return value;
        }""",
     """        let value = hasher.finish();
        if value != 0 {
            return value & 0;
        }""",
     CPU),

    # ---- the backend's own bookkeeping -----------------------------------------------------------
    ("cpu-A23", "A", "a processor id is never recycled, so threads exhaust the monitor", CPU_DYN,
     """        self.shared.release_processor_id(self.processor_id, self.jit.is_null());""",
     """        let _ = self.processor_id;""",
     CPU),

    ("cpu-A24", "A", "a guest access is served without checking the region's protection", ACCESS,
     """    if !permits(region.protection, access) {
        return Err(Refusal::Protection);
    }""",
     """    if false && !permits(region.protection, access) {
        return Err(Refusal::Protection);
    }""",
     MEM_AND_CPU),

    # ---- direction B: over-corrections that read as more careful ----------------------------------
    ("cpu-B2", "B", "TLS blocks committed eagerly so no guest thread ever faults", CPU_TLS,
     """                CommitPolicy::Lazy,""",
     """                CommitPolicy::Eager,""",
     CPU),

    ("cpu-B3", "B", "block linking turned off as well, for a second escape D16 prices at 7x",
     CPU_DYN,
     """        if self.interruptible {
            optimization::INTERRUPTIBLE
        } else {""",
     """        if self.interruptible {
            optimization::INTERRUPTIBLE & !optimization::BLOCK_LINKING
        } else {""",
     CPU),

    ("cpu-B4", "B", "code invalidation refuses a range outside the guest address space", CPU_DYN,
     """    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.executable_cache = None);""",
     """    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        if !self.shared.extent.contains(range.start()) {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "invalidate code outside the guest address space",
                reason: "over-correction: the trait says such a range is not an error",
            });
        }
        self.with_ctx(|ctx| ctx.executable_cache = None);""",
     CPU),

    ("cpu-A35", "A", "the fetch cache ignores whether the region is committed (M5)",
     CPU_CALLBACKS,
     """            .is_some_and(|(start, end, committed)| {
                committed && address >= start && address + 4 <= end
            });""",
     """            .is_some_and(|(start, end, committed)| {
                let _ = committed;
                address >= start && address + 4 <= end
            });""",
     CPU),

    # ---- I3: the guest's architectural counter ---------------------------------------------------
    # There is deliberately no row for leaving `cntfrq_el0` at 0 rather than programming
    # `clock::CNTFRQ_HZ`. dynarmic's default for 0 is the same 600 MHz, so the mutation is a no-op on
    # this pin and would MISS -- and a row that cannot fail is worse than no row. What the explicit
    # programming buys is that the frequency and the scale are one constant; if either moves, the
    # guest-side `the_guest_reads_the_frequency_the_backend_advertises` fails on the exact value.
    ("cpu-A27", "A", "CNTPCT_EL0 goes back to the per-slice instruction counter", CPU_CALLBACKS,
     """    unsafe { with(ctx, 0, |_| crate::clock::cntpct()) }""",
     """    unsafe { with(ctx, 0, |c| c.ticks_used) }""",
     CPU),

    ("cpu-A34", "A", "the counter returns nanoseconds, so its units are not the advertised CNTFRQ",
     CPU_CLOCK,
     """    let ticks = nanos.saturating_mul(u128::from(CNTFRQ_HZ)) / NANOS_PER_SECOND;""",
     """    let ticks = nanos;""",
     CPU),

    ("cpu-B8", "B", "the counter epoch made per host thread, so two guest threads disagree",
     CPU_CLOCK,
     """    let epoch = *EPOCH.get_or_init(Instant::now);""",
     """    thread_local! {
        static THREAD_EPOCH: Instant = Instant::now();
    }
    let epoch = THREAD_EPOCH.with(|e| *e);""",
     CPU),

    # ---- teardown: the three defects with no symptom where they happen ---------------------------
    ("cpu-A25", "A", "GuestTls no longer frees itself, so a failed construction leaks a block",
     CPU_TLS,
     """    fn drop(&mut self) {
        self.free.lock().push(self.base);
    }""",
     """    fn drop(&mut self) {
        let _ = self.base;
    }""",
     CPU),

    ("cpu-B7", "B", "a block is returned twice, so two live guest threads share one stack guard",
     CPU_TLS,
     """        self.free.lock().push(self.base);""",
     """        self.free.lock().push(self.base);
        self.free.lock().push(self.base);""",
     CPU),

    ("cpu-A26", "A", "the processor id is recycled before the jit that holds its monitor entry",
     CPU_DYN,
     """        unsafe { od_jit_free(self.jit) };
        // Nulled so that `self.jit.is_null()` below *is* the statement "the jit is gone" rather
        // than a comment claiming it, and so a use-after-free of this field would be a null
        // dereference rather than a dangling one.
        self.jit = core::ptr::null_mut();
        // Only now: no jit can reference this processor's monitor entry any more.
        self.shared.release_processor_id(self.processor_id, self.jit.is_null());""",
     """        self.shared.release_processor_id(self.processor_id, self.jit.is_null());
        unsafe { od_jit_free(self.jit) };
        self.jit = core::ptr::null_mut();""",
     CPU),

    # There is no row for "run clears a stale halt bit on entry", because there is no such line to
    # revert. The review's M2 asked for one; the emitted dispatcher already does it
    # (`block_of_code.cpp:403-405` ends every return path with `lock xchg` on `halt_reason`), so an
    # entry clear changed nothing and was removed. What replaced it is a row against the pin itself,
    # in `shim-A*`: if dynarmic ever stopped reading-and-clearing, M2 would become real, and that is
    # the thing worth detecting.

    ("cpu-A33", "A", "the pager refusal loses its line continuation again (M1's defect class)",
     CPU_DYN,
     """                        "{e}. D10 requires Omnidroid to take guest faults ahead of dynarmic's own \\
                         handler; without that every guest fault recompiles its block onto the \\
                         callback path, measured 30-49x slower with correct results""",
     """                        "{e}. D10 requires Omnidroid to take guest faults ahead of dynarmic's own                          handler; without that every guest fault recompiles its block onto the                          callback path, measured 30-49x slower with correct results""",
     CPU),

    # ---- the demand pager ------------------------------------------------------------------------
    # ---- the shared access policy (M4) -----------------------------------------------------------
    # These four rows are the ones the review asked for: before the policy was unified, no row could
    # flip a rule on one side and check that the other caught it, because there were two rules. Now
    # there is one, and each of these runs BOTH suites, so a row that only one crate notices is
    # visible as such in the "N test(s)" column.
    ("mem-A15", "A", "the shared policy commits a page the guest may not write", ACCESS,
     """        FaultAccess::Write => protection.is_writable(),""",
     """        FaultAccess::Write => protection.is_readable(),""",
     MEM_AND_CPU),

    ("mem-A18", "A", "the length check dropped, so an access may run off the end of its region",
     ACCESS,
     """    if access_end > region.end() {
        return Err(Refusal::NotMapped);
    }
    if !permits(region.protection, access) {""",
     """    if false && access_end > region.end() {
        return Err(Refusal::NotMapped);
    }
    if !permits(region.protection, access) {""",
     MEM_AND_CPU),

    ("mem-A19", "A", "free address space is admitted, so a wild guest address resolves", ACCESS,
     """) -> Result<(), Refusal> {
    if region.is_free() {""",
     """) -> Result<(), Refusal> {
    if false && region.is_free() {""",
     MEM_AND_CPU),

    # There is no row for dropping the `anonymous &&` from rule 4, and the reason is a finding
    # rather than an omission: it is **inert**. `Inner::commit_range` skips any entry whose OS state
    # is not a placeholder, and a file-backed view never is, so committing "for any kind" commits
    # nothing extra and returns the same 0. The check stays as an early-out and a statement of
    # intent, and it is now written down that the layer below is what enforces it. A row would MISS,
    # and a row that cannot fail is worse than no row.
    ("mem-B7", "B", "rule 4 commits the whole mapping rather than the granule that was touched",
     ACCESS,
     """        match space.ensure_committed(address, len.max(1)) {""",
     """        match space.ensure_committed(region.mapping_start, region.mapping_len) {""",
     MEM_AND_CPU),

    ("mem-A16", "A", "the pager claims faults from outside its own address space", PAGER,
     """    if fault.address < inner.base || fault.address >= inner.end {
        return FaultOutcome::NotOurs;
    }""",
     """    if false {
        return FaultOutcome::NotOurs;
    }""",
     MEM_AND_CPU),

    ("mem-A17", "A", "a zero-byte commit is declined again, so a concurrent fault leaves fastmem",
     PAGER,
     """    if anonymous && !exhausted {
        return FaultOutcome::Resolved;
    }""",
     """    if false && anonymous && !exhausted {
        return FaultOutcome::Resolved;
    }""",
     MEM_AND_CPU),

    ("mem-A20", "A", "the retry record goes back to one address, so two in a granule loop", PAGER,
     """    let granule = fault.address - fault.address % inner.granule.max(1);
    let repeated = LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.replace(granule)) == granule;""",
     """    let granule = fault.address;
    let repeated = LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.replace(granule)) == granule;""",
     MEM_AND_CPU),

    ("mem-A21", "A", "the streak bound removed, so a cycle of distinct granules never terminates",
     PAGER,
     """    let exhausted = repeated || streak > MAX_ZERO_COMMIT_STREAK;""",
     """    let exhausted = repeated;""",
     MEM_AND_CPU),

    ("mem-A22", "A", "examined stops counting the outcome, so declined > examined again", PAGER,
     """        self.examined.fetch_add(1, Ordering::Relaxed);
        match outcome {""",
     """        if !matches!(outcome, FaultOutcome::NotOurs) {
            self.examined.fetch_add(1, Ordering::Relaxed);
        }
        match outcome {""",
     MEM_AND_CPU),

    ("mem-A23", "A", "the pager invents an access length it was never told", PAGER,
     """    match crate::access::admit(&inner.space, fault.address, 1, fault.access) {""",
     """    match crate::access::admit(&inner.space, fault.address, usize::MAX, fault.access) {""",
     MEM_AND_CPU),

    ("mem-B8", "B", "the retry bound tightened to one zero-commit per thread for all time", PAGER,
     """const MAX_ZERO_COMMIT_STREAK: u32 = 1024;""",
     """const MAX_ZERO_COMMIT_STREAK: u32 = 1;""",
     MEM_AND_CPU),
    # ---- .eh_frame: the function map M2's whole choice of code rests on -------------------------
    # ---- C1: the slot is drained, and not reclaimable until the drain finishes -------------------
    # Four rows on the two hazards -- calls in flight, and the slot itself -- and two on what the fix
    # must NOT cost.
    #
    # There is deliberately no row weakening the `SeqCst` accesses to acquire/release, and the reason
    # is NOT that the weakening is safe. It is unsound on this host: the pair is `W(active);R(handler)`
    # against `W(handler);R(active)`, the store-buffer shape, and **StoreLoad is precisely the one
    # reordering x86-64's TSO permits** -- `release`'s plain store to `handler` may sit in the store
    # buffer while its plain load of `active` executes, and a dispatch that has already read the live
    # handler is missed. (`dispatch`'s own side happens to be fenced regardless, because a locked
    # read-modify-write is a full barrier on x86, but that is an accident of the target.) The row is
    # absent because the defect is a *race*: reverting it does not make a test fail, it makes a test
    # fail sometimes, and a flaky row attributes a mutation to the wrong detector (Task 1). An earlier
    # version of this comment said the hardware does not perform that reordering, which was wrong in
    # the reassuring direction -- exactly how the next person weakens it with a clean conscience.
    #
    # There is likewise no row for taking the in-flight reference *after* the handler load rather than
    # before: the window it opens is between two instructions, and no deterministic test can land in
    # it.
    ("plat-A3", "A", "the drain removed, so release returns with a dispatch still in the handler",
     FAULT,
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);""",
     """    if false && slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);""",
     PLATFORM),

    ("plat-A4", "A", "the in-flight reference dropped before the handler call instead of after",
     FAULT,
     """        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        let outcome = handler(context, fault);
        drop(guard);""",
     """        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        drop(guard);
        let outcome = handler(context, fault);""",
     PLATFORM),

    ("plat-A5", "A", "a draining slot is unpublished with zero, so install can take it mid-drain",
     FAULT,
     """    slot.handler.store(DRAINING, Ordering::SeqCst);""",
     """    slot.handler.store(0, Ordering::SeqCst);""",
     PLATFORM),

    ("plat-A6", "A", "the context is cleared before the drain rather than after it", FAULT,
     """    // 2. No call is still *running* after this loop.""",
     """    slot.context.store(0, Ordering::Relaxed);

    // 2. No call is still *running* after this loop.""",
     PLATFORM),

    ("plat-B7", "B", "the drain made a whole-table barrier, so one space waits on another's fault",
     FAULT,
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 {""",
     """    if SLOTS.iter().any(|s| s.active.load(Ordering::SeqCst) != 0) {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while SLOTS.iter().any(|s| s.active.load(Ordering::SeqCst) != 0) {""",
     PLATFORM),

    ("plat-B8", "B", "slots retired rather than reused, the cheaper C1 fix the review offered",
     FAULT,
     """    slot.handler.store(0, Ordering::Release);""",
     """    slot.handler.store(DRAINING, Ordering::Release);""",
     PLATFORM),

    ("elf-A20", "A", "the table's datarel base dropped, so every function start is wrong",
     EH_FRAME,
     """        Apply::DataRelative => hdr_vaddr,""",
     """        Apply::DataRelative => 0,""",
     ELF_SCAN),

    ("elf-A21", "A", "pc_range read with the pointer's base applied, so lengths become addresses",
     EH_FRAME,
     """            fde_encoding & 0x0F,
            0,
            "FDE pc_range",""",
     """            fde_encoding,
            0,
            "FDE pc_range",""",
     ELF_SCAN),

    ("elf-A22", "A", "the table's initial_location is no longer checked against the FDE's pc_begin",
     EH_FRAME,
     """            if bounds.start != initial_location {""",
     """            if false && bounds.start != initial_location {""",
     ELF_SCAN),

    ("elf-A23", "A", "fde_count trusted rather than bounded by the bytes present", EH_FRAME,
     """    if entry_bytes == 0 || fde_count > available / entry_bytes {""",
     """    if false {""",
     ELF_SCAN),

    ("elf-A24", "A", "an unimplemented DWARF pointer encoding is read as udata4 instead of refused",
     EH_FRAME,
     """        _ => {
            return Err(ElfError::UnsupportedEhFrameEncoding { what, encoding });
        }""",
     """        _ => Format { bytes: 4, signed: false },""",
     ELF_SCAN),

    ("elf-A25", "A", "a LEB128 with no terminator is walked without a bound", EH_FRAME,
     """        if used >= 10 {""",
     """        if false {""",
     ELF_SCAN),

    # Direction B: over-corrections that read as more careful and destroy the map.
    ("elf-B10", "B", "a zero-length FDE refused again, so one entry rejects all 245,117", EH_FRAME,
     """        if start.checked_add(len).is_none() {""",
     """        if len == 0 || start.checked_add(len).is_none() {""",
     ELF_SCAN),

    # ---- the leaf classifier ---------------------------------------------------------------------
    ("elf-A26", "A", "a BL is no longer recorded, so a function that calls out grades as a leaf",
     LEAF,
     """        if w & 0xFC00_0000 == 0x9400_0000 {
            facts.direct_calls.insert(branch_target(at, imm26(w)));""",
     """        if w & 0xFC00_0000 == 0x9400_0000 {
            let _ = branch_target(at, imm26(w));""",
     ELF_SCAN),

    ("elf-A27", "A", "a call before the last RET is counted as if it were in the failure tail",
     LEAF,
     """            if last_return.is_none_or(|last| i < last) {
                facts.calls_before_last_return += 1;
            }""",
     """            if false {
                facts.calls_before_last_return += 1;
            }""",
     ELF_SCAN),

    ("elf-A28", "A", "a memory base that is neither SP nor a thread pointer is not recorded", LEAF,
     """                if rn != 31 && !thread_pointer_regs[rn as usize] {
                    facts.foreign_memory_bases.insert(rn);
                }""",
     """                if false {
                    facts.foreign_memory_bases.insert(rn);
                }""",
     ELF_SCAN),

    ("elf-A29", "A", "a thread-pointer register stays one after being redefined", LEAF,
     """            0b1000 | 0b1001 | 0b0101 | 0b1101 => {
                thread_pointer_regs[(w & 0x1F) as usize] = false;
            }""",
     """            0b1000 | 0b1001 | 0b0101 | 0b1101 => {}""",
     ELF_SCAN),

    ("elf-A30", "A", "an undecodable word no longer disqualifies a body", LEAF,
     """        if !self.fully_decoded()""",
     """        if false""",
     ELF_SCAN),

    ("elf-A31", "A", "a function map claiming more code than the object holds is scanned anyway",
     LEAF,
     """    if decoded > executable_bytes {""",
     """    if false {""",
     ELF_SCAN),

    ("elf-B11", "B", "the hint space refused again, losing every padded candidate", LEAF,
     """        if w & 0xFFFF_F01F == 0xD503_201F {
            facts.hints += 1;
            continue;
        }""",
     """        if w & 0xFFFF_F01F == 0xD503_201F {
            facts.system_instructions += 1;
            continue;
        }""",
     ELF_SCAN),

    ("elf-B12", "B", "a stack-guard tail past the last RET refused, so only unprotected code runs",
     LEAF,
     """        if !self.direct_calls.is_empty() {""",
     """        if true {""",
     ELF_SCAN),

    # ---- the per-slice callback invariant ---------------------------------------------------------
    ("cpu-A32", "A", "fastmem_exclusive_access lost, so every LDXR leaves the fast path", CPU_DYN,
     """            fastmem_exclusive_access: 1,""",
     """            fastmem_exclusive_access: 0,""",
     CPU),

    ("cpu-A28", "A", "the per-slice callback delta is no longer checked", CPU_DYN,
     """                let delta = self.slow_path_entries().saturating_sub(before);
                if delta != 0 {""",
     """                let delta = self.slow_path_entries().saturating_sub(before);
                if false && delta != 0 {""",
     CPU),

    ("cpu-A29", "A", "the invariant is never armed, so it can only ever pass", CPU_DYN,
     """        let armed = options.assert_callback_free_slices && shared.owns_guest_paging;""",
     """        let armed = false;""",
     CPU),

    ("cpu-A30", "A", "the exemption widened to every exit, so only a budget expiry can violate it",
     CPU_DYN,
     """                        Some(PendingExit::Returned { .. }) => Some("the guest returned"),""",
     """                        Some(PendingExit::Returned { .. }) => None,""",
     CPU),

    ("cpu-A31", "A", "a failed demand-pager install is swallowed again", CPU_DYN,
     """            Err(e) if e.is_unsupported() => None,""",
     """            Err(e) if true || e.is_unsupported() => None,""",
     CPU),

    ("cpu-B6", "B", "the memory-fault exemption removed, so every real guest fault is a violation",
     CPU_DYN,
     """                        Some(PendingExit::Fault { .. }) => None,""",
     """                        Some(PendingExit::Fault { .. }) => Some("a memory fault"),""",
     CPU),

    # ---- the inline thunk boundary (M3 task 1) ---------------------------------------------------
    ("cpu-A36", "A",
     "the dispatcher stops installing the host MXCSR under an inline thunk handler", CPU_DYN,
     """            let switched = guest != host;
            if switched {
                write(host);
            }""",
     """            let switched = guest != host;""",
     CPU),

    ("cpu-A37", "A",
     "the guard installs the host MXCSR but never puts the guest's back", CPU_DYN,
     """    impl Drop for Guard {
        fn drop(&mut self) {
            if self.switched {
                write(self.guest);
            }
        }
    }""",
     """    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.guest;
        }
    }""",
     CPU),

    # There is deliberately no row for dropping the `&& owns_guest_paging` term from the arming
    # condition. Since `DynarmicBackend::new` now *refuses* a platform that has a vectored handler
    # and could not give us one, `owns_guest_paging` is false only where there is no handler
    # implementation at all -- Linux and macOS -- so on this host the term is hard to make differ
    # and a row written today would MISS. A row that cannot fail is worse than no row (Task 1).
    #
    # It is *closable*, though, and saying otherwise would overstate the obstacle: the precedent is
    # `create_misconfigured_thread`, a `test-support`-gated constructor that builds a context the
    # production path refuses precisely so an unreachable check can be shown to fire. A test-only
    # option that declines to install the pager would do the same here. It is not done because the
    # check is a platform guard rather than a defect anyone has hit, and a new bypass of a safety
    # property is not free -- a judgement about priority, not about possibility.

    # ---- AAPCS64 marshalling (M3 task 2) ---------------------------------------------------------
    # Every row here is a way to be *silently* wrong: each produces a plausible number rather than an
    # error, which is the failure shape 3,594 initializers hide (Global Constraint 1).
    ("abi-A1", "A",
     "the integer bank back-fills after spilling to the stack",
     ABI,
     """        let value = if self.ngrn < ARG_REGISTERS {""",
     """        let value = if self.ngrn <= ARG_REGISTERS {""",
     ANDROID),

    ("abi-A2", "A",
     "the two argument banks share one counter, so a double lands in an X register",
     ABI,
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn);
            self.nsrn += 1;""",
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = u128::from(self.call.x(self.nsrn));
            self.nsrn += 1;""",
     ANDROID),

    ("abi-A3", "A",
     "an int return zero-extended, so every libc -1 reads as success",
     ABI,
     """        self.call.set_x(0, i64::from(value) as u64);""",
     """        self.call.set_x(0, u64::from(value as u32));""",
     ANDROID),

    ("abi-A4", "A",
     "a float return written as a double's bit pattern",
     ABI,
     """    pub fn f32(&mut self, value: f32) {
        self.call.set_v(0, u128::from(value.to_bits()));
    }""",
     """    pub fn f32(&mut self, value: f32) {
        self.call.set_v(0, u128::from(f64::from(value).to_bits()));
    }""",
     ANDROID),

    ("abi-A5", "A",
     "a stack argument read without checking that the guest's stack is there",
     ABI,
     # STALE PATTERN REPAIRED. F1's fix made `align_nsaa` fallible, so the `;` in the original
     # pattern stopped matching the source and this row had been silently reporting a MISS for a
     # reason that had nothing to do with the test it names. Found by a pattern check over the
     # whole table; the row's intent is unchanged.
     """            let at = self.align_nsaa(8)?;
            let value = self.mem.read_u64(at, self.blame())?;""",
     """            let at = self.align_nsaa(8)?;
            let value = self.mem.read_u64(at, self.blame()).unwrap_or(0);""",
     ANDROID),

    # The over-correction: a boundary that refused a zero-length access would refuse
    # `memcpy(dst, src, 0)`, which is legal C and which the engine emits.
    ("abi-B1", "B",
     "a zero-length guest access refused instead of being a no-op",
     ANDROID_MEM,
     """        if len == 0 {""",
     """        if false && len == 0 {""",
     ANDROID),

    # ---- the variadic rules, which are not the fixed rules ---------------------------------------
    ("varargs-A1", "A",
     "the SIMD save area stepped by 8 bytes instead of 16",
     VARARGS,
     """pub const VR_SLOT: usize = 16;""",
     """pub const VR_SLOT: usize = 8;""",
     ANDROID),

    ("varargs-A2", "A",
     "variadic floating point read from the integer registers, which is Windows-on-ARM64's rule",
     VARARGS,
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn) as u64;
            self.nsrn += 1;""",
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.x(self.nsrn);
            self.nsrn += 1;""",
     ANDROID),

    ("varargs-A3", "A",
     "the guest va_list's offsets no longer range-checked",
     VARARGS,
     """        if value < low || value > high {""",
     """        if false && (value < low || value > high) {""",
     ANDROID),

    ("varargs-A4", "A",
     "a save-area pointer plus a negative offset allowed to wrap into the top of the address space",
     VARARGS,
     """        let sum = i128::from(top as u64) + i128::from(offs);""",
     """        let sum = i128::from((top as u64).wrapping_add(offs as u64));""",
     ANDROID),

    # The over-correction: a positive offset is legal and means "the registers are spent", so
    # refusing one refuses a correct guest.
    ("varargs-B1", "B",
     "a positive va_list offset refused instead of normalised",
     VARARGS,
     """        let high = save_bytes as i64;""",
     """        let high = -1;""",
     ANDROID),

    # ---- guest memory, which is hostile by assumption --------------------------------------------
    ("android-mem-A1", "A",
     "a guest string walk no longer bounded by its region's end",
     ANDROID_MEM,
     """        let reach = region_end.saturating_sub(address).min(Self::STRING_LIMIT);""",
     """        let reach = Self::STRING_LIMIT;""",
     ANDROID),

    ("android-mem-A2", "A",
     "guest memory read without admitting the range at all",
     ANDROID_MEM,
     """        self.check(address, len, FaultAccess::Read, blame)?;
        let mut out = vec![0u8; len];""",
     """        let mut out = vec![0u8; len];""",
     ANDROID),

    # ---- the thunk region ------------------------------------------------------------------------
    ("region-A1", "A",
     "the function area made executable, so a mid-slot branch runs whatever is there",
     REGION,
     """            // Not executable, and lazily committed. See the module docs: this is what turns a branch
            // into the middle of a slot into a typed fault instead of four bytes of something.
            Protection::Read,""",
     """            Protection::ReadExecute,""",
     ANDROID),

    # The over-correction, against Global Constraint 6: the function area is never read on the path
    # that works, so committing it up front is commit charge paid for nothing.
    ("region-B1", "B",
     "the function area committed eagerly instead of lazily",
     REGION,
     """            Protection::Read,
            CommitPolicy::Lazy,""",
     """            Protection::Read,
            CommitPolicy::Eager,""",
     ANDROID),

    # ---- the boundary itself ---------------------------------------------------------------------
    ("boundary-A1", "A",
     "an unbound symbol returns quietly instead of naming itself, which is Constraint 1's shape",
     BOUNDARY,
     """            Binding::Unbound => Err(AbiError::Unbound {
                symbol: slot.symbol.clone(),
                address: slot.address,
            }),""",
     """            Binding::Unbound => Ok(resume),""",
     ANDROID),

    ("boundary-A2", "A",
     "the host-to-guest recursion depth no longer capped",
     BOUNDARY,
     """        if depth > MAX_GUEST_DEPTH {""",
     """        if false && depth > MAX_GUEST_DEPTH {""",
     ANDROID),

    ("boundary-A3", "A",
     "a callback restores only the caller-saved registers, leaving X19-X28 clobbered",
     BOUNDARY,
     """        for (index, &value) in self.x.iter().enumerate() {
            cpu.set_x(XReg::new(index as u8).expect("X0-X30 exist"), value);
        }""",
     """        for (index, &value) in self.x.iter().enumerate().take(19) {
            cpu.set_x(XReg::new(index as u8).expect("X0-X30 exist"), value);
        }""",
     ANDROID),

    ("boundary-A4", "A",
     "SP not restored after a call into guest code",
     BOUNDARY,
     """        cpu.set_sp(self.sp);
        cpu.set_pc(self.pc);""",
     """        cpu.set_pc(self.pc);""",
     ANDROID),

    ("boundary-A5", "A",
     "a failing inline handler records its error and lets the guest carry on anyway",
     BOUNDARY,
     """            record_pending(error);
            import.call.defer_to_caller();""",
     """            record_pending(error);""",
     ANDROID),

    ("boundary-A6", "A",
     "the exit path stops reading a deferred error, so it reports the symbol as unbound",
     BOUNDARY,
     """            if let Some(error) = take_pending() {
                return Err(error);
            }""",
     """            if let Some(error) = take_pending() {
                let _ = error;
            }""",
     ANDROID),

    ("boundary-A7", "A",
     "a callback entered on a stack pointer AArch64 forbids",
     BOUNDARY,
     """    let sp = cpu.sp();
    if sp % 16 != 0 {""",
     """    let sp = cpu.sp();
    if false && sp % 16 != 0 {""",
     ANDROID),

    ("boundary-A8", "A",
     "the exit-path crossing cap removed",
     BOUNDARY,
     """            if crossings >= self.exit_crossings {""",
     """            if false && crossings >= self.exit_crossings {""",
     ANDROID),

    ("boundary-A9", "A",
     "an execute fault inside the region no longer re-described, so a mid-slot branch loses its symbol",
     BOUNDARY,
     """                ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                    if self.region.holds_function(address) || self.region.holds_data(address) =>""",
     """                ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                    if false && (self.region.holds_function(address)
                        || self.region.holds_data(address)) =>""",
     ANDROID),

    # The over-correction: the exact-address lookup removed, so every legitimate imported call falls
    # through into the mid-slot machinery and is refused.
    #
    # This replaced a row that was an **equivalent mutant** and correctly reported NOT CAUGHT: turning
    # `if offset != 0` into `if true` changes nothing, because a call to a slot's own address is
    # answered by the exact lookup above and never reaches the guard. The harness was right and the row
    # was wrong, which is the distinction Global Constraint 13 asks for.
    ("boundary-B1", "B",
     "the exact slot lookup removed, so a legitimate call is treated as a branch into a slot",
     BOUNDARY,
     """        if let Some(slot) = self.slots.get(&address) {
            return Ok(slot);
        }""",
     """        if let Some(slot) = self.slots.get(&address) {
            let _ = slot;
        }""",
     ANDROID),

    ("boundary-A10", "A",
     "the caller's budget handed afresh to every crossing instead of spent down",
     BOUNDARY,
     """if let RunLimit::Instructions(allowance) = remaining {""",
     """if let RunLimit::Instructions(allowance) = RunLimit::Unlimited {""",
     ANDROID),

    # The defect the first version of the budget accounting had: charging the allowance after every
    # `cpu.run` and pre-empting on any exit, which turns a `MemoryFault` -- not resumable -- into a
    # `StepLimitReached`, which is.
    ("boundary-A11", "A",
     "the budget pre-empts any exit that lands on its last instruction, not only a crossing",
     BOUNDARY,
     """                other => return Ok(other),""",
     """                other if matches!(remaining, RunLimit::Instructions(n)
                    if n <= cpu.last_run_instructions()) =>
                {
                    return Ok(ExitReason::StepLimitReached { pc: other.pc(), executed: spent })
                }
                other => return Ok(other),""",
     ANDROID),

    # ---- the seam the boundary rests on ----------------------------------------------------------
    ("cpu-A38", "A",
     "a deferred inline thunk resumes the guest anyway instead of exiting",
     CPU_CALLBACKS,
     """                if deferred {""",
     """                if false && deferred {""",
     ANDROID),

    ("cpu-A39", "A",
     "SP missing from the register file a thunk handler sees",
     CPU_DYN,
     """    fn sp(&self) -> GuestAddr {
        // SAFETY: as `x`. `SP` is a field of `JitState` like any other.
        unsafe { od_jit_get_sp(self.jit) as GuestAddr }
    }""",
     """    fn sp(&self) -> GuestAddr {
        0
    }""",
     ANDROID),
    # ---- Task 2 review: the address arithmetic the guest controls (F1) --------------------------
    # Each of these reverts a `checked_add` to the unchecked round-up. In a profile with overflow
    # checks OFF the unchecked form wraps rather than panicking, and the following read then fails
    # with `BadPointer` anyway — so every one of these tests asserts the refused POINTER, not just
    # the variant. A row that only removed the check would otherwise be a MISS in release.
    ("varargs-A5", "A",
     "the va_list __stack round-up is unchecked again, so a top-of-space __stack wraps",
     VARARGS,
     """    fn aligned_stack(&self, align: usize) -> AbiResult<GuestAddr> {
        self.stack
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.stack_out_of_space(self.stack, align))
    }""",
     """    fn aligned_stack(&self, align: usize) -> AbiResult<GuestAddr> {
        Ok((self.stack + align - 1) & !(align - 1))
    }""",
     ANDROID),

    ("varargs-A6", "A",
     "the VarArgs overflow-area round-up is unchecked again",
     VARARGS,
     """    fn aligned_overflow(&self, align: usize) -> AbiResult<GuestAddr> {
        self.overflow
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.overflow_out_of_space(self.overflow, align))
    }""",
     """    fn aligned_overflow(&self, align: usize) -> AbiResult<GuestAddr> {
        Ok((self.overflow + align - 1) & !(align - 1))
    }""",
     ANDROID),

    ("abi-A6", "A",
     "the NSAA round-up is unchecked again, so a top-of-space SP wraps",
     ABI,
     """        self.nsaa
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.nsaa_out_of_space(self.nsaa, align))""",
     """        Ok((self.nsaa + align - 1) & !(align - 1))""",
     ANDROID),

    # ---- Task 2 review: the va_list bank names itself (F5) --------------------------------------
    # The original defect: `core::ptr::eq(&self.gr_top, &top)` with `top` by value is always false,
    # so every general-bank refusal was reported as `__vr_top` with the VR bound. Two rows, because
    # the name and the bound are two separable halves of the same fact.
    ("varargs-A7", "A",
     "every save-area refusal names __vr_top again, whichever bank it came from",
     VARARGS,
     """    fn field(self) -> &'static str {
        match self {
            SaveBank::General => "__gr_top",
            SaveBank::Simd => "__vr_top",
        }
    }""",
     """    fn field(self) -> &'static str {
        "__vr_top"
    }""",
     ANDROID),

    ("varargs-A8", "A",
     "every save-area refusal carries the SIMD bound again, whichever bank it came from",
     VARARGS,
     """    fn save_bytes(self) -> usize {
        match self {
            SaveBank::General => GR_SAVE_BYTES,
            SaveBank::Simd => VR_SAVE_BYTES,
        }
    }""",
     """    fn save_bytes(self) -> usize {
        VR_SAVE_BYTES
    }""",
     ANDROID),

    # ---- Task 2 review: a failed handler is not a step limit (F3) -------------------------------
    # Restores the ordering the late-budget fix left behind: the budget arm above the pending-error
    # check, so a handler that failed on the budget's last instruction is reported as a resumable
    # StepLimitReached and its typed error is dropped by the next run's `let _ = take_pending()`.
    ("boundary-A12", "A",
     "the counted budget is checked before the pending handler error, so a failure is lost",
     BOUNDARY,
     """            if let Some(error) = take_pending() {
                return Err(error);
            }
            // Only now, having decided to go round again, is the allowance spent down. A budget that
            // has run out stops the guest *at the thunk*, unserviced and resumable, which is the
            // honest stop: servicing the call and then refusing to resume would leave the caller
            // unable to say what happened.
            if let RunLimit::Instructions(allowance) = remaining {
                let left = allowance.saturating_sub(cpu.last_run_instructions());
                if left == 0 {
                    return Ok(ExitReason::StepLimitReached { pc: site, executed: spent });
                }
                remaining = RunLimit::Instructions(left);
            }
            crossings += 1;""",
     """            if let RunLimit::Instructions(allowance) = remaining {
                let left = allowance.saturating_sub(cpu.last_run_instructions());
                if left == 0 {
                    return Ok(ExitReason::StepLimitReached { pc: site, executed: spent });
                }
                remaining = RunLimit::Instructions(left);
            }
            crossings += 1;
            if let Some(error) = take_pending() {
                return Err(error);
            }""",
     ANDROID),

    # ---- an access may span entries of one mapping, and only of one mapping --------------------
    # A commit carves the map into entries that are each exactly one OS placeholder, and adjacent
    # committed granules are never coalesced -- so a lazily-committed mapping is a run of entries and
    # an ordinary access can straddle two of them. A1 restores the single-entry check that refused
    # every such access as NotMapped; B1 is the over-correction, letting the walk run out of its
    # mapping into whatever is next.
    ("access-A1", "A",
     "only the first entry is checked, so an access straddling a granule boundary is refused",
     ACCESS,
     """    admits_region(&region, address, access_end.min(covered_end) - address, access)?;""",
     """    admits_region(&region, address, len, access)?;""",
     MEM_AND_CPU),

    ("access-B1", "B",
     "the span walk crosses out of its mapping into whatever is mapped next",
     ACCESS,
     """        if next.start != covered_end || next.mapping.is_none() || next.mapping != region.mapping {""",
     """        if next.start != covered_end || next.is_free() {""",
     MEM_AND_CPU),

    # ---- a scan has no length, so it needs its own reach ----------------------------------------
    # `scan_reach` is the bound a C-string walk uses. A2 is the defect it was written for: bounding
    # the walk by the first entry, which is a granule boundary the guest has never heard of and
    # which killed a guest thread on an ordinary log line. A3 is the same defect from the commit
    # side. B2 is the over-correction -- running out of the mapping into whatever is next -- and B3
    # lets the scan commit memory the guest never asked for, which charges D15's ceiling to look
    # for a NUL.
    ("access-A2", "A",
     "a scan is bounded by its first entry, so a string that crosses a granule is refused",
     ACCESS,
     """    let mut end = first.end;
    while end < ceiling {""",
     """    let mut end = first.end;
    while end < address {""",
     MEM_AND_CPU),

    ("access-A3", "A",
     "the scan ignores the caller's limit and runs to the end of the mapping instead",
     ACCESS,
     """    let ceiling = address.checked_add(limit).unwrap_or(GuestAddr::MAX);""",
     """    let ceiling = GuestAddr::MAX;""",
     MEM_AND_CPU),

    ("access-B2", "B",
     "the scan crosses out of its mapping into whatever is mapped next",
     ACCESS,
     """        if next.start != end
            || next.mapping != region.mapping
            || !next.is_committed()
            || admits_region(&next, end, 1, access).is_err()
        {""",
     """        if next.start != end || next.is_free() {""",
     MEM_AND_CPU),

    ("access-B3", "B",
     "the scan looks into an uncommitted granule, committing it to hunt for a terminator",
     ACCESS,
     """            || !next.is_committed()
            || admits_region(&next, end, 1, access).is_err()""",
     """            || admits_region(&next, end, 1, access).is_err()""",
     MEM_AND_CPU),

    ("access-B4", "B",
     "the scan reads into a range whose protection was dropped under it",
     ACCESS,
     """            || admits_region(&next, end, 1, access).is_err()""",
     """            || next.is_free()""",
     MEM_AND_CPU),

    # ---- omni-bionic ----------------------------------------------------------------------------
    # The crate had 126 rows' worth of workspace mutation coverage around it and NONE of its own,
    # across 12,543 lines. These rows target the claims that would be silently wrong rather than
    # loudly broken: the guest ABI's widths, the Linux errno numbering, and the two error-reporting
    # conventions that are opposites of each other.

    # ---- the frame walk is evidence, and a walk that hangs is not ------------------------------
    # `unwind::frames` reads guest-supplied pointers (D6: assume one that writes its own frame
    # pointer). A1/A2 remove the two guards that make a hostile chain terminate; B1 removes the
    # alignment check that comes before the read.
    ("unwind-A1", "A",
     "the ascent check goes, so a frame pointing at itself walks to the bound every time",
     BIONIC_UNWIND,
     """        if next <= fp {
            break;
        }""",
     """        if next < fp {
            break;
        }""",
     BIONIC),

    ("unwind-A2", "A",
     "a null return address no longer ends the chain, so the list reports zeroes as frames",
     BIONIC_UNWIND,
     """        if ret == 0 {
            break;
        }""",
     """        if ret == u64::MAX {
            break;
        }""",
     BIONIC),

    ("unwind-B1", "B",
     "the alignment check goes, so a misaligned frame pointer is read rather than rejected",
     BIONIC_UNWIND,
     """        if fp == 0 || fp % 8 != 0 {""",
     """        if fp == 0 {""",
     BIONIC),

    # The guest's errno numbers are LINUX numbers. The development host is Windows, whose numbering
    # is different, so a value quietly taken from the host is the classic silent-wrong-answer here.
    ("bionic-A1", "A",
     "ETIMEDOUT becomes Windows' ERROR_SEM_TIMEOUT instead of the Linux value",
     BIONIC_ERRNO,
     """    pub const ETIMEDOUT: i32 = 110;""",
     """    pub const ETIMEDOUT: i32 = 121;""",
     BIONIC),

    ("bionic-A2", "A",
     "EAGAIN renumbered off the kernel's value",
     BIONIC_ERRNO,
     """    pub const EAGAIN: i32 = 11;""",
     """    pub const EAGAIN: i32 = 35;""",
     BIONIC),

    # Guest struct widths. Over-declaring a size is how a write lands past the end of a guest object.
    ("bionic-A3", "A",
     "pthread_mutex_t declared 32 bytes, as if bionic used the glibc-shaped layout",
     BIONIC_LAYOUTS,
     """    pub const PTHREAD_MUTEX_T: u64 = 40;""",
     """    pub const PTHREAD_MUTEX_T: u64 = 32;""",
     BIONIC),

    ("bionic-A4", "A",
     "timespec declared 8 bytes, as if time_t were 32-bit",
     BIONIC_LAYOUTS,
     """    pub const TIMESPEC: u64 = 16;""",
     """    pub const TIMESPEC: u64 = 8;""",
     BIONIC),

    ("bionic-A5", "A",
     "pthread_t declared 32-bit, as if a host thread id could carry it",
     BIONIC_LAYOUTS,
     """    pub const PTHREAD_T: u64 = 8;""",
     """    pub const PTHREAD_T: u64 = 4;""",
     BIONIC),

    # wchar_t is 32-bit on Android, not the 16 bits a Windows-shaped assumption would give it.
    ("bionic-A6", "A",
     "the wide-string walk steps by 2, as if wchar_t were 16-bit",
     BIONIC_WIDE,
     """        count += 1;
        cursor = cursor.checked_add(4).ok_or(Fault(cursor))?;""",
     """        count += 1;
        cursor = cursor.checked_add(2).ok_or(Fault(cursor))?;""",
     BIONIC),

    # The FORTIFY check is an off-by-one away from accepting a string with no room for its NUL.
    ("bionic-A7", "A",
     "__strlen_chk accepts a string exactly filling its object, leaving no room for the NUL",
     BIONIC_STRING,
     """    if len >= size {
        return Err(crate::error::BionicError::CheckFailed("__strlen_chk"));""",
     """    if len > size {
        return Err(crate::error::BionicError::CheckFailed("__strlen_chk"));""",
     BIONIC),

    # The sem waiter-flag protocol. A1 restores the defect that stalled a blocked waiter for a full
    # second; the suite could not see it because every sem_wait loops on a bounded slice.
    ("bionic-A8", "A",
     "sem_post consumes the waiter flag other waiters still need",
     BIONIC_SEM,
     """        let next = word + 1;""",
     """        let next = (word & !sem_bits::WAITERS) + 1;""",
     BIONIC),

    # strtol's overflow reporting: the value clamps AND errno is set. Dropping either is silent.
    ("bionic-A9", "A",
     "strtol overflow clamps but does not report ERANGE",
     BIONIC_NUMERICS,
     """    if overflow {
        ctx.set_errno(crate::errno::consts::ERANGE);
        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     """    if overflow {
        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     BIONIC),

    ("bionic-A10", "A",
     "strtol overflow clamps to LONG_MAX regardless of sign",
     BIONIC_NUMERICS,
     """        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     """        return Ok(Ok(i64::MAX));""",
     BIONIC),

    # The over-correction: bionic's compare functions return the BYTE DIFFERENCE, not glibc's plus or
    # minus one. Only the sign is specified by C, so this reads as a harmless normalisation -- which
    # is exactly why it needs a row.
    ("bionic-B1", "B",
     "strcmp normalised to glibc's plus-or-minus one instead of bionic's byte difference",
     BIONIC_STRING,
     """        // Bionic's strcmp returns the byte difference (c - d), not glibc's ±1. The C
        // standard only fixes the SIGN; bionic fixes the magnitude. We match bionic.
        Some((ca, cb)) => Ok(ca as i32 - cb as i32),""",
     """        Some((ca, cb)) => Ok(if ca > cb { 1 } else { -1 }),""",
     BIONIC),
    # ---- the printf engine's two bounds on guest-chosen sizes ----------------------------------
    # A width is guest-controlled and `emit_padded` pads with `repeat_n`, so an unbounded one is an
    # allocation the guest picked. These four rows are the pair of bounds in both directions.
    ("printf-A1", "A", "the per-conversion field-width cap removed", BIONIC_PRINTF,
     """                if n > MAX_FIELD_WIDTH {""",
     """                if false && n > MAX_FIELD_WIDTH {""",
     BIONIC),

    ("printf-B1", "B", "the field-width cap tightened below a width real code uses", BIONIC_PRINTF,
     """pub const MAX_FIELD_WIDTH: usize = 64 * 1024;""",
     """pub const MAX_FIELD_WIDTH: usize = 64;""",
     BIONIC),

    ("printf-A2", "A", "the total-output cap removed, so a repeated wide field is unbounded",
     BIONIC_PRINTF,
     """        if out.len() - start_len > MAX_OUTPUT {""",
     """        if false && out.len() - start_len > MAX_OUTPUT {""",
     BIONIC),

    ("printf-B2", "B", "the total-output cap lowered below an ordinary formatted result",
     BIONIC_PRINTF,
     """pub const MAX_OUTPUT: usize = 1024 * 1024;""",
     """pub const MAX_OUTPUT: usize = 8;""",
     BIONIC),

    # ---- F6: the long double refusal, which must fire and must not over-fire -------------------
    ("printf-A3", "A", "the %Lf refusal removed, so a 128-bit quad is read as a double",
     BIONIC_PRINTF,
     """            if length == "L" {""",
     """            if false && length == "L" {""",
     BIONIC),

    ("printf-B3", "B", "the %Lf refusal widened to `l`, refusing the legal %lf", BIONIC_PRINTF,
     """            if length == "L" {""",
     """            if length == "L" || length == "l" {""",
     BIONIC),

    # ---- the one parser: plan and format must agree argument for argument ----------------------
    # `format` fetches a `*` width before it looks at the conversion character, `%%` included. A
    # planner that skipped it diverges by one argument and every later conversion prints the NEXT
    # argument -- a plausible wrong answer, not an error.
    ("printf-A4", "A", "plan skips a `*` width on %%, diverging from format by one argument",
     BIONIC_PRINTF,
     """        if spec.width == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.precision == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.conv == '%' {
            continue;
        }""",
     """        if spec.conv == '%' {
            continue;
        }
        if spec.width == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.precision == Count::Star {
            kinds.push(ArgKind::Int);
        }""",
     BIONIC),

    # ---- the adapter: the guest's atomics ------------------------------------------------------
    # A 32-bit atomic on an unaligned address is undefined behaviour in Rust, and AArch64's
    # LDXR/STXR fault there too -- so the refusal is what the guest would see on a real device.
    # Note what A1 does: with the check gone the mutated build performs an unaligned atomic, which
    # is exactly the undefined behaviour the check exists to prevent. It is detected by the
    # refusal test, not by the access misbehaving.
    ("adapter-A1", "A", "the compare-and-swap alignment refusal removed", ADAPTER_VIEW,
     """        if at % 4 != 0 {""",
     """        if false && at % 4 != 0 {""",
     ANDROID),

    ("adapter-B1", "B", "the CAS alignment tightened to 8, refusing a legal 4-aligned mutex",
     ADAPTER_VIEW,
     """        if at % 4 != 0 {""",
     """        if at % 8 != 0 {""",
     ANDROID),

    # ---- the adapter: the LP64 return traps ----------------------------------------------------
    ("adapter-A2", "A", "an int return zero-extended instead of sign-extended", ADAPTER_HANDLERS,
     """    ($c:ident, i32, $v:expr) => {
        $c.ret().i32($v)
    };""",
     """    ($c:ident, i32, $v:expr) => {
        $c.ret().u64($v as u32 as u64)
    };""",
     ANDROID),

    # The over-correction, and it is the one that reads as correct: the C standard fixes only the
    # SIGN of a comparison, so clamping to -1/0/1 looks defensible. bionic fixes the magnitude and
    # guest code can see it.
    ("adapter-B2", "B", "the compare result normalised to its sign, losing bionic's magnitude",
     ADAPTER_HANDLERS,
     """    fn strcmp(a: ptr, b: ptr) -> i32 = |v| omni_bionic::string::strcmp(&v, a, b);""",
     """    fn strcmp(a: ptr, b: ptr) -> i32 =
        |v| omni_bionic::string::strcmp(&v, a, b).map(|d| d.signum());""",
     ANDROID),

    # ---- the adapter: per-thread state ---------------------------------------------------------
    ("adapter-A3", "A", "__errno points at the scratch buffer instead of the errno cell",
     ADAPTER_VIEW,
     """    pub fn errno_address(&self) -> GuestAddr {
        self.active.block + ERRNO_OFFSET
    }""",
     """    pub fn errno_address(&self) -> GuestAddr {
        self.active.block + SCRATCH_OFFSET
    }""",
     ANDROID),

    ("adapter-B3", "B", "the thread arena tightened below a thread count the runtime uses",
     ADAPTER_MOD,
     """pub const MAX_GUEST_THREADS: usize = 256;""",
     """pub const MAX_GUEST_THREADS: usize = 4;""",
     ANDROID),

    # ---- the adapter: the printf family --------------------------------------------------------
    # snprintf returns what WOULD have been written. A handler returning the truncated length makes
    # every caller that grows its buffer on overflow loop forever, and every short result is
    # identical either way -- so the defect is invisible until a result is truncated.
    ("adapter-A4", "A", "snprintf returns the truncated length instead of the full one",
     ADAPTER_FORMAT,
     """    view.mem().write_bytes(at, &write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(full)""",
     """    view.mem().write_bytes(at, &write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(i32::try_from(room).unwrap_or(full))""",
     ANDROID),

    ("adapter-A5", "A", "a variadic int taken as 64 bits instead of narrowed to its own width",
     ADAPTER_FORMAT,
     """            ArgKind::Int => Owned::Int(source.next_u64()? as u32 as i32 as i64),""",
     """            ArgKind::Int => Owned::Int(source.next_u64()? as i64),""",
     ANDROID),

    ("adapter-A6", "A", "a null %s argument dereferenced instead of printing (null)",
     ADAPTER_FORMAT,
     """                let pointer = source.next_u64()?;
                if pointer == 0 {
                    Owned::NullStr
                } else {
                    Owned::Str(read_latin1(view.blaming(argument), pointer, argument)?)
                }""",
     """                let pointer = source.next_u64()?;
                Owned::Str(read_latin1(view.blaming(argument), pointer, argument)?)""",
     ANDROID),


    # ---- the rwlock's futex contract ------------------------------------------------------------
    # Both loops used to hand the futex a literal 0 while the word is non-zero by construction. The
    # crate's mock and the adapter both ignore `expected`, so the placeholder was invisible --
    # `mutex` and `once` pass real values, only `rwlock` and `sem` did not. A futex that DOES compare
    # would answer WouldBlock to every waiter and the `continue` would busy spin.
    ("bionic-A11", "A",
     "a blocking reader hands the futex a placeholder instead of the word it read",
     BIONIC_RWLOCK,
     """        match futex.wait(rwlock_addr, state, Some(remaining)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => {
                if deadline.is_none() {
                    continue; // protocol re-check (no lost wake under the policy)
                }""",
     """        match futex.wait(rwlock_addr, 0, Some(remaining)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => {
                if deadline.is_none() {
                    continue; // protocol re-check (no lost wake under the policy)
                }""",
     BIONIC),

    # The over-correction is the ORIGINAL value: a net so wide it is itself the stall.
    ("bionic-B2", "B",
     "the self-heal slice widened back to a second, so a lost wake is a one-second stall",
     BIONIC_RWLOCK,
     """const SELF_HEAL_SLICE: Duration = Duration::from_millis(50);""",
     """const SELF_HEAL_SLICE: Duration = Duration::from_millis(1_000);""",
     BIONIC),

    # ---- M3 task 3 phase 2: the dl* family ------------------------------------------------------
    #
    # `dl_iterate_phdr` is the one import in this phase that cannot be a stub: the C++ runtime in
    # `libroblox.so` is statically linked, so the in-guest unwinder walks 11.5 MB of `.eh_frame`
    # through it and every `throw` depends on the answer.
    ("dl-A1", "A",
     "dl_iterate_phdr reports an empty process instead of refusing when nothing is registered",
     ADAPTER_DL,
     """    if images.is_empty() {""",
     """    if false && images.is_empty() {""",
     ANDROID),

    # The struct layout, which is the silently-wrong class here: a callback reading `dlpi_phdr` out
    # of where `dlpi_name` was written gets a pointer that is not a program header table.
    ("dl-A2", "A", "dl_phdr_info's dlpi_phdr written at +8, on top of dlpi_name", ADAPTER_DL,
     """    pub(super) const PHDR: usize = 16;""",
     """    pub(super) const PHDR: usize = 8;""",
     ANDROID),

    ("dl-A3", "A", "the walk does not stop when a callback answers non-zero", ADAPTER_DL,
     """        if returned != 0 {
            result = returned;
            break;
        }""",
     """        if returned != 0 {
            result = returned;
        }""",
     ANDROID),

    ("dl-A4", "A", "a null dl_iterate_phdr callback is called instead of refused", ADAPTER_DL,
     """    let target = usize::try_from(callback).ok().filter(|&t| t != 0).ok_or_else(|| {""",
     """    let target = usize::try_from(callback).ok().ok_or_else(|| {""",
     ANDROID),

    # The over-correction: a walk that always stops after one object. The unwinder would then never
    # see any library but the first, which is a *success* returning the first callback's answer.
    ("dl-B1", "B", "the walk stops after the first object whatever the callback answered",
     ADAPTER_DL,
     """        if returned != 0 {
            result = returned;
            break;
        }""",
     """        {
            result = returned;
            break;
        }""",
     ANDROID),

    # ---- M3 task 3 phase 2: the eighteen data objects -------------------------------------------
    # RE-TARGETED in phase 3b: the loop this mutated gained a `register_stream` call, so the
    # original pattern stopped matching and the pre-flight refused the whole run. That is the gate
    # working -- a stale row is otherwise a MISS that looks like a missing test.
    ("data-A1", "A", "stdin/stdout/stderr spaced by a pointer instead of by a whole FILE",
     ADAPTER_DATA,
     """        let stream = sf + index * FILE_BYTES;""",
     """        let stream = sf + index * 8;""",
     ANDROID),

    ("data-A2", "A", "in6addr_loopback is 1:: rather than ::1", ADAPTER_DATA,
     """    ones[15] = 1;""",
     """    ones[0] = 1;""",
     ANDROID),

    # A zero canary compares equal to a zeroed stack slot, so a stack overflow that wrote zeroes
    # passes every `__stack_chk_fail` check. `omni-cpu` refuses to generate one; this is the other
    # half of that refusal.
    ("data-A3", "A", "a zero stack canary is stored instead of refused", ADAPTER_DATA,
     """    if process.stack_guard == 0 {""",
     """    if false && process.stack_guard == 0 {""",
     ANDROID),

    ("data-A4", "A", "environ is null rather than pointing at an empty vector", ADAPTER_DATA,
     """    mem.write_u64(environ, empty_environ as u64, blame("environ", environ))?;""",
     """    mem.write_u64(environ, 0, blame("environ", environ))?;""",
     ANDROID),

    ("data-A5", "A", "__sF is one FILE wide, so stdout and stderr are somebody else's object",
     ADAPTER_DATA,
     """    DataObject { symbol: "__sF", len: 3 * FILE_BYTES, align: 8 },""",
     """    DataObject { symbol: "__sF", len: FILE_BYTES, align: 8 },""",
     ANDROID),

    # The over-correction: the static pool tightened until the data the phase actually places no
    # longer fits. A bound that refuses correct input is as wrong as no bound.
    ("data-B1", "B", "the static pool tightened below what the eighteen data objects need",
     ADAPTER_MOD,
     """pub const POOL_BYTES: usize = 4096;""",
     """pub const POOL_BYTES: usize = 64;""",
     ANDROID),

    # ---- M3 task 3 phase 2: the guest-memory group ----------------------------------------------
    #
    # Task 2 review F9: these five reach the whole `GuestSpace`, so they are on the exit path. That
    # is also the only path that can reach a CPU, which is what `invalidate` needs.
    ("guestmem-A1", "A",
     "translated code is not discarded when the memory it came from is unmapped or reprotected",
     ADAPTER_GUESTMEM,
     """    if len == 0 {
        return Ok(());
    }
    c.invalidate_code(address, len)""",
     """    if true || len == 0 {
        return Ok(());
    }
    c.invalidate_code(address, len)""",
     ANDROID),

    # **Re-aimed, not retired.** This row used to inject `MADV_DONTNEED` being *answered*, and
    # that is what it now does by design (D28 amendment 1): the guarantee is met by decommitting
    # rather than by writing zeroes, so the old form no longer describes a defect. What is left of
    # the original property is `MADV_REMOVE`, which punches a hole in an *underlying object* that
    # no mapping here has. `jni-A11` and `jni-B3` cover MADV_DONTNEED's own semantics from both
    # directions.
    ("guestmem-A2", "A", "MADV_REMOVE is answered instead of refused", ADAPTER_GUESTMEM,
     """    if advice == MADV_REMOVE {""",
     """    if false {""",
     ANDROID),

    # Re-anchored for M3's gate: the `fd != -1` half of this condition was a defect in its own
    # right (`guestmem-A10`), and removing it left this row's pattern unmatched. The statement is
    # unchanged -- serving a file-backed request as anonymous memory hands the guest zeroed pages
    # where it asked for a file's contents, and succeeds while doing it.
    ("guestmem-A3", "A", "a file-backed mmap is served as anonymous memory", ADAPTER_GUESTMEM,
     """    if flags & MAP_ANONYMOUS == 0 {""",
     """    if false {""",
     ANDROID),

    # Widening rather than refusing: the guest asked for write-only and is given read as well.
    ("guestmem-A4", "A", "PROT_WRITE alone widened to ReadWrite instead of refused",
     ADAPTER_GUESTMEM,
     """        p if p == PROT_READ | PROT_WRITE => Ok(Protection::ReadWrite),""",
     """        p if p == PROT_READ | PROT_WRITE || p == PROT_WRITE => Ok(Protection::ReadWrite),""",
     ANDROID),

    # Global Constraint 11's "saturating arithmetic on a limit turns hostile input into a larger
    # permission", in the one place in this phase where a guest length is rounded up.
    ("guestmem-A5", "A", "a length rounded up to a page saturates instead of being checked",
     ADAPTER_GUESTMEM,
     """    len.checked_add(mask).map(|n| n & !mask)""",
     """    Some(len.saturating_add(mask) & !mask)""",
     ANDROID),

    ("guestmem-A6", "A", "mlock answers 0, which is the believable wrong answer", ADAPTER_GUESTMEM,
     """    let call = Call::begin(c)?;
    call.refuse(format!(
        "the guest asked to lock {len} bytes at {addr:#x} into memory.""",
     """    let call = Call::begin(c)?;
    c.ret(|mut r| r.i32(0));
    return Ok(());
    #[allow(unreachable_code)]
    call.refuse(format!(
        "the guest asked to lock {len} bytes at {addr:#x} into memory.""",
     ANDROID),

    # The over-correction: refusing a request that is correct here. MAP_SHARED on anonymous memory
    # differs from MAP_PRIVATE only across a `fork`, and there is none.
    ("guestmem-B1", "B", "an anonymous MAP_SHARED refused although there is no fork to share with",
     ADAPTER_GUESTMEM,
     """    if !matches!(flags & MAP_TYPE, MAP_PRIVATE | MAP_SHARED) {""",
     """    if !matches!(flags & MAP_TYPE, MAP_PRIVATE) {""",
     ANDROID),

    # The over-correction that destroys the measured design property: D10's "never commit
    # speculatively", and the demand pager being the heap seam rather than `malloc`.
    ("guestmem-B2", "B", "a guest mmap commits eagerly, so the heap seam stops being the pager",
     ADAPTER_GUESTMEM,
     """    match space.map_anonymous(placement, len, protection, CommitPolicy::Lazy) {""",
     """    match space.map_anonymous(placement, len, protection, CommitPolicy::Eager) {""",
     ANDROID),

    ("guestmem-B3", "B", "the purely advisory madvise hints refused as well", ADAPTER_GUESTMEM,
     """    if ADVISORY.contains(&advice) {""",
     """    if false && ADVISORY.contains(&advice) {""",
     ANDROID),
    # ---- M3 task 3 phase 3a: the OS surface ------------------------------------------------------
    #
    # `omni-platform` grows past `vm` and `fault` for the first time. Two halves, and both are
    # mutated: the seam itself (clock, process, log) and the twenty-three guest symbols over it.

    # The monotonic clock re-anchored per call. Still non-decreasing, still plausible, and every
    # reading is ~0 -- so a guest measuring an interval measures nothing.
    ("seam-A1", "A", "the monotonic clock is re-anchored on every call instead of on one epoch",
     PLAT_CLOCK,
     """    let epoch = *EPOCH.get_or_init(Instant::now);""",
     """    let epoch = Instant::now();""",
     PLATFORM),

    # The worst available value out of an entropy source: a buffer of zeroes, reported as filled.
    ("seam-A2", "A", "random_bytes reports success without asking the OS for anything", PLAT_PROCESS,
     """        let status = unsafe {
            BCryptGenRandom(core::ptr::null_mut(), chunk.as_mut_ptr(), len, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };""",
     """        let _ = (&chunk, len);
        let status = 0;""",
     PLATFORM),

    ("seam-A3", "A", "sleep returns immediately whatever it was asked for", PLAT_CLOCK,
     """    if duration.is_zero() {
        return;
    }""",
     """    if true {
        return;
    }""",
     PLATFORM),

    ("seam-A4", "A", "an out-of-range android log priority is mapped to a neighbour", PLAT_LOG,
     """            8 => Priority::Silent,
            _ => return None,""",
     """            8 => Priority::Silent,
            _ => Priority::Unknown,""",
     PLATFORM),

    # The over-correction: an empty request is a no-op in C and must not become a failure.
    ("seam-B1", "B", "random_bytes fails an empty request instead of treating it as a no-op",
     PLAT_PROCESS_MOD,
     """    if out.is_empty() {
        return Ok(());
    }""",
     """    if out.is_empty() {
        return Err(ProcessError::Status {
            operation: "random_bytes",
            api: "BCryptGenRandom",
            status: -1,
        });
    }""",
     PLATFORM),

    # The over-correction: a severity that maps perfectly well is refused.
    ("seam-B2", "B", "the most severe syslog level stops mapping onto the android scale", PLAT_LOG,
     """            0..=2 => Priority::Fatal,""",
     """            1..=2 => Priority::Fatal,""",
     PLATFORM),

    # ---- the calendar ----------------------------------------------------------------------------
    #
    # Every row here is wrong only for part of the input range, which is what makes the conversion
    # worth mutating at all: a wrong answer for 1969 and a right one for 2026 is exactly the shape
    # that ships.

    ("time-A1", "A", "floor division becomes truncating, so every pre-1970 date is a day late",
     BIONIC_TIME,
     """    if numerator % denominator != 0 && ((numerator < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }""",
     """    quotient""",
     BIONIC),

    ("time-A2", "A", "the Gregorian 400-year leap exception is dropped, so 2000 is not a leap year",
     BIONIC_TIME,
     """    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0""",
     """    year % 4 == 0 && year % 100 != 0""",
     BIONIC),

    ("time-A3", "A", "a year that will not fit int tm_year wraps instead of reporting EOVERFLOW",
     BIONIC_TIME,
     """    let Ok(tm_year) = i32::try_from(tm_year) else {
        return Err(GmtimeError::YearOutOfRange { year });
    };""",
     """    let tm_year = tm_year as i32;""",
     BIONIC),

    ("time-A4", "A", "the weekday uses % instead of rem_euclid, so pre-1970 days are negative",
     BIONIC_TIME,
     """    let wday = (days + UNIX_EPOCH_WEEKDAY).rem_euclid(7);""",
     """    let wday = (days + UNIX_EPOCH_WEEKDAY) % 7;""",
     BIONIC),

    ("time-A5", "A", "tm_yday loses the leap-day adjustment after February", BIONIC_TIME,
     """    let leap_day = i32::from(is_leap(year) && month > 2);""",
     """    let leap_day = 0;""",
     BIONIC),

    ("time-A6", "A", "tm_zone is left null, so guest code prints a const char * that is not there",
     BIONIC_TIME,
     """    bytes[TM_ZONE_OFFSET..TM_ZONE_OFFSET + 8].copy_from_slice(&zone.to_le_bytes());""",
     """    let _ = zone;""",
     BIONIC),

    # The over-correction: refusing input that is entirely legal. A negative time_t is a date
    # before 1970, not an error.
    ("time-B1", "B", "gmtime refuses every pre-1970 timestamp", BIONIC_TIME,
     """    let days = floor_div(timestamp, SECONDS_PER_DAY);""",
     """    if timestamp < 0 {
        return Err(GmtimeError::YearOutOfRange { year: 0 });
    }
    let days = floor_div(timestamp, SECONDS_PER_DAY);""",
     BIONIC),

    # The over-correction: the year bound tightened below what the guest's own `int` allows.
    ("time-B2", "B", "the tm_year bound is narrowed to 16 bits, refusing years an int holds",
     BIONIC_TIME,
     """    let Ok(tm_year) = i32::try_from(tm_year) else {""",
     """    let Ok(tm_year) = i16::try_from(tm_year).map(i32::from) else {""",
     BIONIC),

    # ---- the clock symbols -----------------------------------------------------------------------

    ("clocks-A1", "A", "CLOCK_MONOTONIC is served from the wall clock", ADAPTER_CLOCKS,
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
                omni_platform::clock::monotonic_now()
            }""",
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
                omni_platform::clock::realtime_now()
            }""",
     ANDROID),

    # CLOCK_BOOTTIME counts time spent suspended and the host's monotonic clock does not, so
    # aliasing it is a wrong answer rather than a coarser right one.
    ("clocks-A2", "A", "CLOCK_BOOTTIME is aliased to the monotonic clock instead of refused",
     ADAPTER_CLOCKS,
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {""",
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {""",
     ANDROID),

    ("clocks-A3", "A", "gettimeofday writes nanoseconds into a struct timeval's tv_usec",
     ADAPTER_CLOCKS,
     """            write_pair(&view, tv, seconds, nanos / 1_000, 0)?;""",
     """            write_pair(&view, tv, seconds, nanos, 0)?;""",
     ANDROID),

    ("clocks-A4", "A", "nanosleep accepts a malformed timespec instead of reporting EINVAL",
     ADAPTER_CLOCKS,
     """    if seconds < 0 || !(0..NANOS_PER_SECOND).contains(&nanos) {""",
     """    if false {""",
     ANDROID),

    # **Scoped to the library target on purpose.** Removing the cap makes the end-to-end test sleep
    # for the i64::MAX seconds it asks for, which hangs rather than fails -- the failure mode this
    # harness's own docstring records from M3 task 2. The detector is the unit test on `capped`,
    # which is why that predicate is a function.
    ("clocks-A5", "A", "the sleep cap is not applied, so a guest can block a host thread forever",
     ADAPTER_CLOCKS,
     """    duration.as_secs() > MAX_SLEEP_SECONDS""",
     """    false && duration.as_secs() > MAX_SLEEP_SECONDS""",
     ANDROID_LIB),

    ("clocks-A6", "A", "usleep reads all 64 bits of X0 although useconds_t is 32", ADAPTER_CLOCKS,
     """    let micros = u64::from(c.args().next_u64()? as u32);""",
     """    let micros = c.args().next_u64()?;""",
     ANDROID),

    # The pattern carries `gmtime_r`'s own `Ok` arm because M4 bound `gmtime` beside it and the
    # two share these two lines. Without the context it matched **twice**, and the pre-flight
    # refused the whole run rather than mutating whichever came first -- which is what that gate
    # is for. `clocks-A10` is the same property for the non-reentrant spelling.
    ("clocks-A7", "A", "gmtime_r returns its buffer after failing to fill it", ADAPTER_CLOCKS,
     """                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = guest_address(view.blaming(1), result)?;""",
     """                view.set_errno(EOVERFLOW);
                result
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = guest_address(view.blaming(1), result)?;""",
     ANDROID),

    # **`gmtime` returning its own buffer after failing to fill it**, which is `clocks-A7`'s
    # property for the spelling that owns the storage. A caller that tests the result against NULL
    # -- which is the whole of `gmtime`'s error reporting -- would read a `struct tm` nothing
    # wrote. Bound by M4's gate, so it had no row until now. (Numbered A10: A8 and A9 were both taken, and
    # the harness's duplicate-id gate is what said so before anything ran.)
    ("clocks-A10", "A", "gmtime returns its per-thread struct tm after failing to fill it",
     ADAPTER_CLOCKS,
     """                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = view.tm_address();""",
     """                view.set_errno(EOVERFLOW);
                view.tm_address() as u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = view.tm_address();""",
     ANDROID),

    # The over-correction: the cap applied to the sub-second part, so an ordinary 10 ms sleep is
    # refused. A bound that refuses correct input is as wrong as no bound.
    ("clocks-B1", "B", "the sleep cap is applied to the nanoseconds, refusing a 10 ms sleep",
     ADAPTER_CLOCKS,
     """    duration.as_secs() > MAX_SLEEP_SECONDS""",
     """    u64::from(duration.subsec_nanos()) > MAX_SLEEP_SECONDS""",
     ANDROID),

    # ---- process and environment -----------------------------------------------------------------
    #
    # The first row is the one that matters most in this phase: the open AT_HWCAP decision made by
    # defaulting, which is precisely what the policy type exists to prevent.
    ("procenv-A1", "A",
     "the open AT_HWCAP decision is made by defaulting an instance to Decline", ADAPTER_MOD,
     """            hwcap: Mutex::new(HwcapPolicy::Undecided),""",
     """            hwcap: Mutex::new(HwcapPolicy::Decline),""",
     ANDROID),

    # Re-anchored for M3's gate. `sysconf` answers the page size and the processor count now, so
    # the old form of this row -- answering an unverified constant -- is what the code does. What
    # is still worth injecting is `_SC_PHYS_PAGES`: a number IS available for it, and it is the
    # host's physical memory rather than the guest's budget, which is the same wrong answer
    # `sysinfo` refuses for.
    ("procenv-A2", "A", "sysconf answers _SC_PHYS_PAGES with the host's memory", ADAPTER_PROCENV,
     """        other => {
            let believed = believed_sysconf_name(other).map_or_else(""",
     """        other if other == 0x0062 => 1 << 19,
        other => {
            let believed = believed_sysconf_name(other).map_or_else(""",
     ANDROID),

    ("procenv-A3", "A", "prctl answers 0, which every option has available as a believable done",
     ADAPTER_PROCENV,
     """    if option != PR_SET_VMA {
        let named = prctl_option_name(option)""",
     """    if option != PR_SET_VMA {
        c.ret().i32(0);
        return Ok(());
        #[allow(unreachable_code)]
        let named = prctl_option_name(option)""",
     ANDROID),

    ("procenv-A4", "A", "syscall answers -1/ENOSYS, which callers route around silently",
     ADAPTER_PROCENV,
     """    let named = syscall_name(number).map_or_else(String::new, |name| format!(" (arm64 `{name}`)"));""",
     """    c.ret().i32(-1);
    return Ok(());
    #[allow(unreachable_code)]
    let named = syscall_name(number).map_or_else(String::new, |name| format!(" (arm64 `{name}`)"));""",
     ANDROID),

    # Half a buffer of real entropy and a reported failure: the caller cannot tell which half.
    # **Re-anchored in M6's network phase.** `getentropy` and the raw `getrandom` both validate
    # their destination with the same one-liner, so the pattern went from one match to three and
    # the whole-table pre-flight reported it stale -- `VERIFICATION.md` entry 8's own lesson,
    # arriving again. The anchor now carries the comment above the call, which is unique to
    # `arc4random_buf` and is where the reasoning for the check lives.
    ("procenv-A5", "A",
     "arc4random_buf validates one byte instead of the whole destination", ADAPTER_PROCENV,
     """            // reported failure — and the caller would have no way to know which half it got.
            view.mem().checked_ptr(at, len, true, blame)?;""",
     """            // reported failure — and the caller would have no way to know which half it got.
            view.mem().checked_ptr(at, 1, true, blame)?;""",
     ANDROID),

    ("procenv-A6", "A", "__system_property_get reports the length including its NUL",
     ADAPTER_PROCENV,
     """        i32::try_from(text.len()).map_err(|_| {""",
     """        i32::try_from(text.len() + 1).map_err(|_| {""",
     ANDROID),

    ("procenv-A7", "A", "abort returns to the guest instead of becoming a typed outcome",
     ADAPTER_PROCENV,
     """    let state = active(c.symbol(), c.address())?;
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the guest called abort()",""",
     """    let state = active(c.symbol(), c.address())?;
    c.ret().void();
    return Ok(());
    #[allow(unreachable_code)]
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the guest called abort()",""",
     ANDROID),

    ("procenv-A8", "A", "_exit loses the status the guest asked to exit with", ADAPTER_PROCENV,
     """    Err(AbiError::GuestExited {
        symbol: c.symbol().to_string(),
        address: c.address(),
        status,
    })""",
     """    let _ = status;
    Err(AbiError::GuestExited {
        symbol: c.symbol().to_string(),
        address: c.address(),
        status: 0,
    })""",
     ANDROID),

    ("procenv-A9", "A",
     "the abort message is dropped, so the only account of the crash is lost", ADAPTER_PROCENV,
     """        why: "the guest called abort()",
        message: state.bionic.abort_message(),""",
     """        why: "the guest called abort()",
        message: { let _ = &state; None },""",
     ANDROID),

    # The over-correction: getenv(NULL) is undefined in C, and NULL is the answer that cannot be
    # mistaken for a value. Refusing it fails a program that is merely careless.
    ("procenv-B1", "B", "getenv refuses a null name instead of answering NULL", ADAPTER_PROCENV,
     """        if name == 0 {
            0
        } else {""",
     """        if name == 0 {
            return Err(view.refusal("a null name"));
        } else {""",
     ANDROID),

    # The over-correction: an unset property is a fact, and 0 with an empty string is what bionic
    # answers. Refusing it turns "this host has no property service" into a halt.
    ("procenv-B2", "B", "an unset system property is refused instead of answered as unset",
     ADAPTER_PROCENV,
     """        let text = found.unwrap_or_default();""",
     """        let Some(text) = found else {
            return Err(view.refusal("no such property"));
        };""",
     ANDROID),

    # ---- the log sink ----------------------------------------------------------------------------

    ("logging-A1", "A", "syslog drops the facility instead of carrying it into the tag",
     ADAPTER_LOGGING,
     """            (Some(ident), facility) => format!("{ident}[facility {facility}]"),""",
     """            (Some(ident), _facility) => ident,""",
     ANDROID),

    ("logging-A2", "A", "closelog leaves openlog's ident in place", ADAPTER_LOGGING,
     """    let state = active(c.symbol(), c.address())?;
    state.bionic.set_syslog_ident(None);
    c.ret().void();""",
     """    let state = active(c.symbol(), c.address())?;
    let _ = &state;
    c.ret().void();""",
     ANDROID),

    # **RE-ANCHORED in M5**, when review finding M3 moved the ring's own state out of
    # `bionic/mod.rs` and into `logging::LogRing` so that it could bound bytes as well as records.
    # The property and the detector are unchanged; only the line holding it moved.
    ("logging-A3", "A", "the capture ring drops its newest records rather than its oldest",
     ADAPTER_LOGGING,
     """            let Some(gone) = state.records.pop_front() else { break };""",
     """            let Some(gone) = state.records.pop_back() else { break };""",
     ANDROID),

    # The over-correction: a ring too small to hold what one run produces is a bound that destroys
    # the thing it was bounding.
    ("logging-B1", "B", "the log capture ring is tightened to four records", ADAPTER_MOD,
     """pub const LOG_CAPTURE_MAX: usize = 256;""",
     """pub const LOG_CAPTURE_MAX: usize = 4;""",
     ANDROID),

    # ---- the cond test's hang guard must stay a hang guard ---------------------------------------
    # `signal_wakes_exactly_one` counts how many waiters finished after one signal. Each waiter's
    # timeout is a HANG GUARD; when it was 400 ms it could fire inside the observation window --
    # registration polls four threads at 10 ms a turn -- and a second thread finished on its own
    # timeout rather than on the signal. Seen three times, once laundering itself into a `wcslen`
    # mutation's catch list. The row restores the racing value; the in-test relation catches it
    # deterministically, with no timing dependence of its own.
    ("bionic-B3", "B",
     "the cond hang guard shrunk back to a value that can fire while the count is taken",
     BIONIC_COND,
     """    const WAITER_HANG_GUARD: Duration = Duration::from_secs(5);""",
     """    const WAITER_HANG_GUARD: Duration = Duration::from_millis(400);""",
     BIONIC),
    # ---- phase 3b: the confinement -------------------------------------------------------------
    # The rules in `fs::path` are the whole of what stops a guest opening an arbitrary host file,
    # and the APK under test is cheat-injected (D6). Each row removes one of them.

    ("fs-A1", "A", "the lexical `..` pop removed, so a traversal reaches the host", PLAT_FS_PATH,
     """                components.pop();""",
     """                components.push(String::from(".."));""",
     PLATFORM),

    ("fs-A2", "A", "component hygiene accepts everything: backslashes, drives, device names",
     PLAT_FS_PATH,
     """pub fn hostile_component(name: &str) -> Option<String> {
    if let Some(bad) = name.chars().find(|c| c.is_control()) {""",
     """pub fn hostile_component(name: &str) -> Option<String> {
    if true {
        return None;
    }
    if let Some(bad) = name.chars().find(|c| c.is_control()) {""",
     PLATFORM),

    # The over-correction, and it is the one a "be strict" instinct produces: refusing every path
    # that contains `..` rather than absorbing it. `/a/../b` is an ordinary path a compiler emits.
    ("fs-B1", "B", "a path containing `..` is refused outright rather than absorbed",
     PLAT_FS_PATH,
     """            ".." => {""",
     """            ".." => {
                return Err(FsError::confined(operation, &shown, "no dot-dot"));""",
     PLATFORM),

    # The other over-correction: a rule wide enough to refuse `libroblox.so`.
    ("fs-B2", "B", "component hygiene widened until an ordinary filename is refused",
     PLAT_FS_PATH,
     """    if name.ends_with('.') || name.ends_with(' ') {""",
     """    if name.contains('.') || name.ends_with(' ') {""",
     PLATFORM),

    # ---- phase 3b: the platform seam ------------------------------------------------------------

    # MEASURED and this row is the defect itself: `FileExt::seek_read` on Windows MOVES the file
    # pointer, so a `pread` built on it alone leaves the next sequential read at end of file --
    # every call `Ok`, nothing reported.
    ("fs-A3", "A", "pread stops restoring the descriptor's own offset", PLAT_FS_WINDOWS,
     """    let restored = handle.seek(SeekFrom::Start(saved));""",
     """    let restored = if true { Ok(0u64) } else { handle.seek(SeekFrom::Start(saved)) };""",
     PLATFORM),

    # `st_ino` zero makes every `(st_dev, st_ino)` identity test answer "the same file", which is
    # the worst answer a `stat` has available.
    ("fs-A4", "A", "st_ino becomes a constant, so every file is the same file", PLAT_FS,
     """    if hash == 0 {
        1
    } else {
        hash
    }""",
     """    let _ = hash;
    0""",
     PLATFORM),

    ("fs-A5", "A", "the descriptor ceiling removed, so a leaking guest holds host handles",
     PLAT_FS,
     """        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let mut table = self.table();
        if table.open.len() >= MAX_OPEN_FILES {""",
     """        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let mut table = self.table();
        if false && table.open.len() >= MAX_OPEN_FILES {""",
     PLATFORM),

    ("fs-A6", "A", "unlink removes a directory, which is rmdir's job", PLAT_FS,
     """        if metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::IsADirectory,
                "unlink does not remove directories; rmdir does",
            ));
        }""",
     """        if false {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::IsADirectory,
                "unlink does not remove directories; rmdir does",
            ));
        }""",
     PLATFORM),

    # The over-correction: a directory bound small enough to refuse an ordinary directory.
    ("fs-B3", "B", "the directory-entry ceiling tightened to two entries", PLAT_FS,
     """pub const MAX_DIR_ENTRIES: usize = 65_536;""",
     """pub const MAX_DIR_ENTRIES: usize = 2;""",
     PLATFORM),

    # ---- phase 3b: the `FILE *` layer in omni-bionic ---------------------------------------------

    # `size * nmemb` is two guest numbers. A release build WRAPS, and the wrapped value (zero)
    # still satisfies "fewer items than asked for" -- so the detector is the errno, not the count.
    ("stdio-A1", "A", "fread's size*nmemb multiplication wraps instead of being checked",
     BIONIC_STDIO,
     """    let Some(total) = size.checked_mul(nmemb) else {
        stream.error = true;
        ctx.set_errno(consts::EINVAL);
        return Ok(0);
    };
    if total == 0 {
        // C: zero items, and the stream is untouched. Not an error, and `size == 0` is the case
        // that would divide by zero below.
        return Ok(0);
    }""",
     """    let total = size.wrapping_mul(nmemb);
    if total == 0 {
        return Ok(0);
    }""",
     BIONIC),

    # The `\\n` is escaped because this pattern is Python source **and** Rust source: an unescaped
    # `\n` here is a newline in the pattern rather than the two characters the Rust file holds,
    # and it matches nothing. The pre-flight pattern gate caught it, which is what it is for.
    ("stdio-A2", "A", "fgets reads past its newline instead of stopping on it", BIONIC_STDIO,
     """                if byte[0] == b'\\n' {
                    break;
                }""",
     """                if false {
                    break;
                }""",
     BIONIC),

    ("stdio-A3", "A", "fgets turns an end of file into an empty line", BIONIC_STDIO,
     """    if written == 0 && stream.eof {""",
     """    if false && written == 0 && stream.eof {""",
     BIONIC),

    ("stdio-A4", "A", "fputc returns the argument, so writing 0xff reads as EOF", BIONIC_STDIO,
     """        Ok(1) => i32::from(byte),""",
     """        Ok(1) => c,""",
     BIONIC),

    ("stdio-A5", "A", "a short read no longer sets the end-of-file flag", BIONIC_STDIO,
     """            Ok(0) => {
                stream.eof = true;
                break;
            }
            Ok(got) => {""",
     """            Ok(0) => {
                break;
            }
            Ok(got) => {""",
     BIONIC),

    # The over-correction: bounding `fgets` by the transfer chunk rather than chunking through it,
    # which silently truncates any line longer than 4 KiB.
    ("stdio-B4", "B", "fgets truncates at one transfer chunk instead of chunking through it",
     BIONIC_STDIO,
     """    let capacity = (size as u32 - 1) as u64;""",
     """    let capacity = ((size as u32 - 1) as u64).min(TRANSFER_CHUNK as u64);""",
     BIONIC),

    # And the other one: refusing `fflush` because this layer cannot promise durability. It can
    # not promise durability, and `fflush` does not ask it to -- that is `fsync`.
    ("stdio-B5", "B", "fflush refuses rather than reporting the contract it does meet",
     BIONIC_STDIO,
     """    match descriptors.flush(stream.fd) {
        Ok(()) => 0,""",
     """    match descriptors.flush(stream.fd) {
        Ok(()) => EOF,""",
     BIONIC),

    # ---- phase 3b: the adapter -------------------------------------------------------------------

    # An unclassified host failure given a specific errno is the plausible-wrong-answer class:
    # guest code retries EIO and moves on, and nothing anywhere says what really happened.
    ("files-A1", "A", "an unclassified host failure is given EIO instead of refusing by name",
     ADAPTER_FILES,
     """        _ => return None,
    })
}""",
     """        _ => consts::EIO,
    })
}""",
     ANDROID),

    ("files-A2", "A", "struct stat's st_size moves onto __pad1", ADAPTER_FILES,
     """    put64(48, stat.size, &mut out);""",
     """    put64(40, stat.size, &mut out);""",
     ANDROID),

    ("files-A3", "A", "access(X_OK) answers 0 instead of refusing", ADAPTER_FILES,
     """        if mode & X_OK != 0 {
            return Err(view.refusal(""",
     """        if false {
            return Err(view.refusal(""",
     ANDROID),

    ("files-A4", "A", "the open flags whose guarantees cannot be met are accepted silently",
     ADAPTER_FILES,
     """        if flags & bit == bit {
            return Err(view.refusal(format!(""",
     """        if false && flags & bit == bit {
            return Err(view.refusal(format!(""",
     ANDROID),

    ("files-A5", "A", "__open_2 stops refusing O_CREAT, which bionic's FORTIFY build aborts on",
     ADAPTER_FILES,
     """        if flags & O_CREAT != 0 || flags & O_TMPFILE == O_TMPFILE {""",
     """        if false {""",
     ANDROID),

    ("files-A6", "A", "readdir answers NULL for a wild DIR pointer, which reads as an empty \
directory", ADAPTER_FILES,
     """        let Some(id) = state.bionic.dir_for(dirp) else {""",
     """        let Some(id) = state.bionic.dir_for(dirp).or(Some(-1)) else {""",
     ANDROID),

    # The over-correction: refusing W_OK as well as X_OK, on the same "Windows has no POSIX
    # permissions" argument. It is the same argument and it is wrong there, because a write probe
    # is an exact answer where an execute probe has none.
    ("files-B6", "B", "access refuses W_OK as well as X_OK", ADAPTER_FILES,
     """        if mode & X_OK != 0 {""",
     """        if mode & (X_OK | W_OK) != 0 {""",
     ANDROID),

    # And the one a "mode is not applied" worry produces: refusing every mkdir that asks for
    # permissions this layer cannot set, which stops the engine creating any directory.
    ("files-B7", "B", "mkdir refuses a mode it cannot apply instead of recording that it cannot",
     ADAPTER_FILES,
     """    let (path, _mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.mkdir(&bytes))? {""",
     """    let (path, mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if mode != 0o777 {
            return Err(view.refusal("a mode this layer cannot apply"));
        }
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.mkdir(&bytes))? {""",
     ANDROID),

    # A `FILE *` this instance never handed out is a wild pointer, a use-after-fclose, or the
    # `&__sF[n]` arithmetic an unverified `sizeof(FILE)` would get wrong. Answering it as an
    # ordinary invalid stream lets guest code route around all three.
    ("stdio-A6", "A", "an unknown FILE pointer is answered instead of refused", ADAPTER_STDIO,
     """    view.active.bionic.stream_of(file).ok_or_else(|| {""",
     """    view.active.bionic.stream_of(file).or(Some(Stream::new(-1))).ok_or_else(|| {""",
     ANDROID),

    # The stream's flags live host-side and have to be written back, or `feof` answers false
    # forever after an end of file. The failure is invisible to anything that does not read the
    # flag after an operation changed it.
    ("stdio-A7", "A", "a stream's flags are never written back, so feof never becomes true",
     ADAPTER_MOD,
     """            if let Some(slot) = self.streams.lock().get_mut(&at) {
                *slot = stream;
            }""",
     """            if let Some(slot) = self.streams.lock().get_mut(&at) {
                let _ = (slot, stream);
            }""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: signals

    # `sigfillset` is the one signal symbol that can be answered exactly, and what it answers is
    # the bits. A set of zeroes is an EMPTY set: every range check, every return value and every
    # errno stays exactly as it was, and the guest is told the opposite of what it asked for.
    ("signals-A1", "A", "sigfillset produces an empty set instead of a full one", BIONIC_SIGNAL,
     """pub const FILLED_BYTE: u8 = 0xFF;""",
     """pub const FILLED_BYTE: u8 = 0x00;""",
     BIONIC),

    # The size is the other half. bionic's LP64 `sigset_t` is one `unsigned long`; a four-byte
    # write leaves signals 33-64 clear in a set the guest was told was full.
    ("signals-A2", "A", "sigfillset fills half the set, leaving signals 33-64 clear",
     BIONIC_LAYOUTS,
     """    pub const SIGSET_T: u64 = 8;""",
     """    pub const SIGSET_T: u64 = 4;""",
     BIONIC),

    # The plausible stub this phase exists to refuse: `sigaction` returning 0 tells the guest a
    # handler is installed, and nothing is observable until the fault it registered for happens.
    ("signals-A3", "A", "sigaction reports that a handler was installed", ADAPTER_SIGNALS,
     """    let shape = if act == 0 {""",
     """    if act != u64::MAX {
        c.ret().i32(0);
        return Ok(());
    }
    let shape = if act == 0 {""",
     ANDROID),

    # The over-correction: refusing `sigfillset` too, on the same "there is no signal delivery
    # here" argument. It is the same argument and it is wrong there, because `sigfillset` is a
    # total function of its one argument and needs no signal state at all.
    ("signals-B1", "B", "sigfillset is refused along with the rest of the family",
     ADAPTER_SIGNALS,
     """    let set = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;""",
     """    let set = c.args().next_u64()?;
    if set != u64::MAX {
        return Err(refuse(c, "there is no signal delivery here".to_string()));
    }
    let state = active(c.symbol(), c.address())?;""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: thread lifecycle

    # An instance with no thread host was never configured. EAGAIN says a resource ran out, which
    # is a condition a correct guest retries -- for ever, because nothing will ever free it.
    ("threads-A1", "A", "pthread_create with no thread host reports EAGAIN instead of refusing",
     ADAPTER_THREADS,
     """    let Some(host) = bionic.thread_host() else {
        return call.refuse(""",
     """    let Some(host) = bionic.thread_host() else {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
        #[allow(unreachable_code)]
        return call.refuse(""",
     ANDROID),

    # **The defect thread lifecycle created and nothing else would have noticed.** Taking the
    # block index from the table's LENGTH is exact only while nothing is ever removed, and this
    # phase removes one at every thread exit: remove the entry holding index 1 from a table of
    # three and the next thread is handed index 2, which is live. Two guest threads then share
    # one `errno` cell, and the symptom is an occasional wrong error number in a thread that did
    # nothing wrong.
    ("threads-A2", "A", "an exited thread's block is handed to a thread that is still using one",
     ADAPTER_RUNTIME,
     """        if let Some(index) = self.free.pop() {
            return Some(index);
        }
        if self.high_water >= capacity {
            return None;
        }
        let index = self.high_water;
        self.high_water += 1;
        Some(index)""",
     """        let index = self.slots.len();
        if index >= capacity {
            return None;
        }
        self.high_water = self.high_water.max(index + 1);
        Some(index)""",
     ANDROID),

    # ---- FUTEX_WAIT's comparison and the bucket lock ---------------------------------------------
    # MEASURED as a 180 s gate freeze, every thread blocked and the CPU flat. The comparison ran
    # inside `parking_lot_core::park`'s `validate`, under the futex's bucket lock, and it read the
    # word THROUGH THE ADDRESS SPACE, whose map lock is a `parking_lot::Mutex`. A contended mutex
    # parks on its own address; when that shares the futex's bucket, or the table grows, the
    # waiter blocks on a bucket it holds. The fix admits the word before the park and compares with
    # one atomic load. A1 puts the admission back under the bucket lock. The detector runs its body
    # in a child process, because the deadlock it provokes leaves a parking_lot bucket held for
    # good, and in this process that would hang the rest of the suite instead of failing one test.
    ("futexlock-A1", "A", "the futex word is admitted through the space inside validate again",
     ADAPTER_RUNTIME,
     """        admit()?;
        // Step 2. SAFETY: `admit` returned `Ok`, which is this function's precondition for
        // `word_holds`.
        let still_expected = || unsafe { word_holds(addr, expected) };""",
     """        // Step 2. SAFETY: `admit` returned `Ok`, which is this function's precondition for
        // `word_holds`.
        let still_expected = move || admit().is_ok() && unsafe { word_holds(addr, expected) };""",
     ANDROID_LIB),

    # The over-correction: "validate may not take locks, so compare BEFORE the park and park
    # unconditionally." It reads as the same fix with less in the callback, and it reopens the
    # lost-wake window FUTEX_WAIT exists to close: a word that changes between the comparison and
    # the queue is slept on. The detector holds the word's bucket from a second park, so the change
    # lands in exactly that window every time rather than when the scheduler allows it.
    ("futexlock-B1", "B", "the futex word is compared before the park instead of under its lock",
     ADAPTER_RUNTIME,
     """        let still_expected = || unsafe { word_holds(addr, expected) };""",
     """        if !unsafe { word_holds(addr, expected) } {
            return Ok(WaitResult::WouldBlock);
        }
        let still_expected = || true;""",
     ANDROID_LIB),

    # ---- a guest thread's stack outlives a death this layer caused ------------------------------
    # MEASURED in M6: a worker sampling a table of per-thread records read one on the stack of a
    # thread this layer had killed, found it unmapped, and died of a MemoryFault filed as a failure
    # of its own. bionic frees a stack only after its thread EXITED, and Linux cannot stop one
    # thread mid-function. So only a thread that returned with its destructors run gives its stack
    # back. A1 unmaps every stack again. A2 forgets that teardown skips the destructors.
    ("threadstack-A1", "A", "a killed or stopped guest thread's stack is unmapped under its siblings",
     ADAPTER_THREADS,
     """    let given_back = if exited { bionic.space_ref().unmap(base, len) } else { Ok(()) };""",
     """    let given_back = if true || exited { bionic.space_ref().unmap(base, len) } else { Ok(()) };""",
     ANDROID),

    ("threadstack-A2", "A", "a thread returning during teardown gives back a stack its skipped destructors left pointed into",
     ADAPTER_THREADS,
     """    let exited = matches!(state, GuestThreadState::Returned(_)) && !bionic.guest_threads_stopping();""",
     """    let exited = matches!(state, GuestThreadState::Returned(_));""",
     ANDROID),

    # The over-correction: never give a stack back. It passes every "keeps its stack" test and
    # leaks about a megabyte of address space for every thread the engine creates and joins.
    ("threadstack-B1", "B", "no guest thread ever gives its stack back",
     ADAPTER_THREADS,
     """    let given_back = if exited { bionic.space_ref().unmap(base, len) } else { Ok(()) };""",
     """    let given_back = if false && exited { bionic.space_ref().unmap(base, len) } else { Ok(()) };""",
     ANDROID),

    # A thread that stopped without returning produced no `void *`. Reporting 0 with an untouched
    # `retval` is indistinguishable from a thread that returned NULL, which is the one answer the
    # guest cannot tell apart from success.
    ("threads-A3", "A", "joining a thread that faulted reports success", ADAPTER_MOD,
     """            GuestThreadState::Failed(why) => Err(format!(""",
     """            GuestThreadState::Failed(_why) if start_routine != usize::MAX => {
                Ok(JoinOutcome::Returned(0))
            }
            GuestThreadState::Failed(why) => Err(format!(""",
     ANDROID),

    # `pthread_attr_setstacksize(attr, SIZE_MAX)` is two guest numbers meeting a page size. The
    # masked round-up wraps to ZERO, silently in release, and the thread gets a stack made
    # entirely of its guard page.
    ("threads-A4", "A", "a SIZE_MAX stack request wraps to a zero-byte stack", ADAPTER_THREADS,
     """    value.checked_add(to - remainder)""",
     """    Some(value.wrapping_add(to - remainder))""",
     ANDROID),

    # Without the thunk table on the new context, the new thread's first imported call branches
    # into a region that is not executable. The thread dies rather than calling anything, which
    # nothing about `pthread_create`'s own return value would show.
    ("threads-A5", "A", "the thunk table is not installed on the new thread's context",
     ADAPTER_THREADS,
     """    if let Err(error) = boundary.install(&mut *cpu) {""",
     """    if let Err(error) = (if entry == usize::MAX { boundary.install(&mut *cpu) } else { Ok(()) }) {""",
     ANDROID),

    # The start routine's own `RET` is what ends a guest thread, and it ends it by landing on the
    # boundary's sentinel. Without it the thread returns to whatever `X30` held and runs off.
    ("threads-A6", "A", "the new thread's X30 is not the boundary's sentinel", ADAPTER_THREADS,
     """        cpu.set_x(XReg::new(30).expect("X30 exists"), boundary.sentinel() as u64);""",
     """        cpu.set_x(XReg::new(30).expect("X30 exists"), 0);""",
     ANDROID),

    # The death context: what a dying thread held, taken before its stack is unmapped. Each of
    # these leaves the record looking plausible -- a context with one register, or an empty stack
    # snapshot that reads like a thread whose SP was unreadable, or bytes from the wrong frame.
    ("deathctx-A1", "A", "a dead thread's registers are not kept", ADAPTER_THREADS,
     """    let mut registers: Vec<u64> =
        (0..31).map(|n| cpu.x(XReg::new(n).expect("X0..X30 exist"))).collect();""",
     """    let mut registers: Vec<u64> = Vec::new();""",
     ANDROID),
    ("deathctx-A2", "A", "a dead thread's stack bytes are dropped", ADAPTER_THREADS,
     """            Ok(word) => stack_bytes.extend_from_slice(&word.to_le_bytes()),""",
     """            Ok(_) => break,""",
     ANDROID),
    # `modf`'s fraction carries x's sign (Annex F): without the copysign a negative integer's
    # fraction is +0.0, which reads as correct in every test that only compares values.
    ("modf-A1", "A", "modf's fraction loses x's sign for a negative integer", LIBM,
     """    Ok((x - integral).copysign(x))""",
     """    Ok(x - integral)""",
     BIONIC_LIBM),
    # `getpagesize` must be the same figure `sysconf` and `getauxval` answer.
    ("getpagesize-A1", "A", "getpagesize answers a different figure from sysconf", ADAPTER_PROCENV,
     """    c.ret().i32(page);""",
     """    c.ret().i32(page * 2);""",
     ANDROID),
    # A returning thread's destructors: skipped, the per-thread registry entry an exited
    # thread's `thread_local` would have removed is left dangling for another thread to read.
    ("exitdtor-A1", "A", "a returning thread's destructors are not run", ADAPTER_THREADS,
     """                run_exit_destructors(&bionic, &boundary, &mut *cpu, slot.id, &mut death)
                    .unwrap_or(GuestThreadState::Returned(value))""",
     """                GuestThreadState::Returned(value)""",
     ANDROID),
    # SIGPIPE's recorded action is what a query returns: a query answering a fixed SIG_DFL would
    # hand libcurl's restore the wrong action after a SIG_IGN, and read as correct at first query.
    ("sigpipe-A1", "A", "a SIGPIPE query ignores the recorded action", ADAPTER_SIGNALS,
     """                    &*held,""",
     """                    &[0u8; SIGACTION_BYTES],""",
     ANDROID),
    ("sigpipe-B1", "B", "a SIGPIPE handler is recorded as though it could be delivered", ADAPTER_SIGNALS,
     """            (handler <= SIG_IGN).then_some(action)""",
     """            Some(action)""",
     ANDROID),
    ("ferror-A1", "A", "ferror never reports the error indicator", BIONIC_STDIO,
     """pub const fn ferror(stream: &Stream) -> i32 {
    if stream.error {""",
     """pub const fn ferror(stream: &Stream) -> i32 {
    if stream.error && false {""",
     BIONIC),
    # `localeconv`'s chars are CHAR_MAX ("unspecified"); zero would say "0 fraction digits,
    # currency symbol after the value", a believable locale that is not the C one.
    ("lconv-A1", "A", "the lconv char fields are zero instead of CHAR_MAX", BIONIC_LOCALE,
     """        out[80 + k] = value.to_le_bytes()[0];""",
     """        out[80 + k] = 0 * value.to_le_bytes()[0];""",
     ANDROID),
    ("atfork-A1", "A", "__register_atfork answers 0 and keeps nothing", ADAPTER_PROCENV,
     """        .push(registration);""",
     """        .truncate(0);""",
     ANDROID),
    # `mbrtoc32`'s state: without the write, a character split across calls cannot be finished --
    # the second call sees continuation bytes with an initial state and answers EILSEQ.
    ("mbrtoc32-A1", "A", "a split character's bytes are not kept in the guest's mbstate_t",
     BIONIC_WIDE,
     """        ctx.write(state + (bytes_so_far + i) as u64, &[byte])?;""",
     """        let _ = (state, bytes_so_far, i, byte);""",
     BIONIC),
    # `UDP_SEGMENT`: each of these is a send that reads as success on the sender's side -- one
    # oversized datagram, a 64-segment batch refused, a malformed control message obeyed.
    ("gso-A1", "A", "a UDP_SEGMENT send goes out as one datagram", ADAPTER_NET,
     """        if segment_size == 0 || bytes.len() <= segment_size {""",
     """        if true || segment_size == 0 || bytes.len() <= segment_size {""",
     ANDROID),
    ("gso-A2", "A", "the 64-segment limit refuses the 64th", ADAPTER_NET,
     """        if bytes.len() > segment_size * UDP_MAX_SEGMENTS {""",
     """        if bytes.len() >= segment_size * UDP_MAX_SEGMENTS {""",
     ANDROID),
    ("gso-B1", "B", "a UDP_SEGMENT cmsg of the wrong length is obeyed", ADAPTER_NET,
     """                    if message.kind != UDP_SEGMENT || message.len != UDP_SEGMENT_CMSG_LEN {""",
     """                    if message.kind != UDP_SEGMENT {""",
     ANDROID),
    ("gso-B2", "B", "a control header past msg_controllen is walked", ADAPTER_NET,
     """        if len < 16 || len > controllen - offset as u64 {""",
     """        if len < 16 {""",
     ANDROID),
    # `posix_fallocate`: a file shortened by a call that promises only to grow it, a zero length
    # answered as success, and a read-only descriptor allocated through.
    ("fallocate-A1", "A", "posix_fallocate shortens a file whose range ends inside it", PLAT_FS_WINDOWS,
     """    if end > size {""",
     """    if end != size {""",
     ANDROID),
    ("fallocate-A2", "A", "a zero length is not EINVAL", ADAPTER_FILES,
     """        if offset < 0 || len <= 0 {
            consts::EINVAL""",
     """        if offset < 0 || len < 0 {
            consts::EINVAL""",
     ANDROID),
    ("fallocate-B1", "B", "a read-only descriptor is allocated through", PLAT_FS,
     """            Some(Entry::File { writable: false, .. }) => {
                refuse(FsErrorKind::BadDescriptor, "not open for writing (EBADF)")""",
     """            Some(Entry::File { writable: false, readable: false, .. }) => {
                refuse(FsErrorKind::BadDescriptor, "not open for writing (EBADF)")""",
     ANDROID),
    # `recvmmsg`: a batch that reads as received -- no length, no sender -- or a batch that fails
    # whole because its second datagram had not arrived.
    ("recvmmsg-A1", "A", "msg_len is not written", ADAPTER_NET,
     """                    view.mem().write_u32(guest_address(view, entry + 56)?, length as u32, blame)?;""",
     """                    let _ = (entry, length, blame);""",
     ANDROID),
    ("recvmmsg-A2", "A", "msg_namelen is left as the caller's room", ADAPTER_NET,
     """        view.mem().write_u32(at + 8, len as u32, blame)?;""",
     """        let _ = len;""",
     ANDROID),
    ("recvmmsg-B1", "B", "a failure after the first message fails the whole batch", ADAPTER_NET,
     """                Netted::Failed(_) => break,""",
     """                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),""",
     ANDROID),
    # An oversized datagram's WSAEMSGSIZE: without the raw-code fallback it reaches the guest as an
    # unclassified refusal, and ngtcp2's path-MTU discovery dies on what it would have handled.
    ("msgsize-A1", "A", "a raw code std cannot classify is left Other", "crates/omni-platform/src/net/error.rs",
     """                error.raw_os_error().map_or(NetErrorKind::Other, super::backend::kind_from_raw)""",
     """                error.raw_os_error().map_or(NetErrorKind::Other, |_| NetErrorKind::Other)""",
     PLATFORM),
    # Image format properties: a transposed pair of the five scalars answers a different question
    # with the right shape; a driver failure written over the guest's buffer is a plausible answer.
    ("imgfmt-A1", "A", "tiling and image type are transposed on the way to the host", "crates/omni-android/src/vulkan/physical.rs",
     """        image_type: args[2] as u32 as i32,
        tiling: args[3] as u32 as i32,""",
     """        image_type: args[3] as u32 as i32,
        tiling: args[2] as u32 as i32,""",
     VULKAN),
    ("imgfmt-B1", "B", "a driver failure is written over the guest's buffer", "crates/omni-android/src/vulkan/physical.rs",
     """    match host.physical_device_image_format_properties(device, query)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(bytes) => {""",
     """    match host.physical_device_image_format_properties(device, query)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.mem().write_bytes(guest_pointer(at, "p", args[6])?, &[0u8; IMAGE_FORMAT_PROPERTIES_BYTES], c.blame(6))?;
            c.ret().i32(result);
        }
        DriverAnswer::Ok(bytes) => {""",
     VULKAN),
    # The format a query names is `w1`; read from `x2` it asks about the image type's number.
    ("vkformat-A1", "A", "the format is read from the wrong register", "crates/omni-android/src/vulkan/physical.rs",
     """    let format = args[1] as u32 as i32;
    let bytes = host.physical_device_format_properties(device, format)?;""",
     """    let format = args[2] as u32 as i32;
    let bytes = host.physical_device_format_properties(device, format)?;""",
     VULKAN),
    # pNext chains: members written over the guest's own header, or a device created without the
    # features its chain enabled -- both look like success to the call that did it.
    ("vkchain-A1", "A", "a chained structure's answer is written over its sType and pNext", "crates/omni-android/src/vulkan/chain.rs",
     """        c.mem().write_bytes(*link_at + CHAIN_HEADER_BYTES as GuestAddr, &link.body, c.blame(argument))?;""",
     """        c.mem().write_bytes(*link_at, &link.body, c.blame(argument))?;""",
     VULKAN),
    ("vkchain-A2", "A", "vkCreateDevice drops the guest's chain", "crates/omni-android/src/vulkan/device.rs",
     """    Ok(DeviceRequest { flags: u32_at(16), queues, layers, extensions, features, chain })""",
     """    let _ = chain;
    Ok(DeviceRequest { flags: u32_at(16), queues, layers, extensions, features, chain: Vec::new() })""",
     VULKAN),
    # A read-only file mapping: the file's bytes are the whole point, and a mapping left writable
    # is one the guest can corrupt believing it cannot.
    ("mmapfile-A1", "A", "a file mapping is not filled from the file", "crates/omni-android/src/bionic/guestmem.rs",
     """        call.mem.write_bytes(at, &host[..read], blame)?;""",
     """        let _ = (&host, read, blame);""",
     ANDROID),
    ("mmapfile-B1", "B", "a PROT_READ file mapping is left writable", "crates/omni-android/src/bionic/guestmem.rs",
     """        if let Err(error) = space.protect(at, len, Protection::Read) {""",
     """        if let Err(error) = space.protect(at, len, Protection::ReadWrite) {""",
     ANDROID),
    # **A kept, measured MISS**, for sockcfg-A3's reason: with the arm gone, the read goes to the
    # host, and Windows refuses reading a handle opened write-only with ERROR_ACCESS_DENIED -- which
    # this seam maps to the same EACCES. No test on this host can separate the check from the host's
    # own refusal; the arm is what keeps Linux's order (EACCES before any read) on a host that would
    # not refuse.
    ("mmapfile-B2", "B", "a descriptor not open for reading is mapped", PLAT_FS,
     """            Some(Entry::File { readable: false, guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::PermissionDenied,
                "a file mapping of a descriptor not open for reading (EACCES)",""",
     """            Some(Entry::File { readable: false, writable: false, guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::PermissionDenied,
                "a file mapping of a descriptor not open for reading (EACCES)",""",
     ANDROID),
    # Query pools: a request with two members transposed creates a pool of the wrong size or
    # type; a destroy that does not forget leaves a handle that still names a destroyed pool.
    ("vkquery-A1", "A", "queryType and queryCount are transposed", "crates/omni-android/src/vulkan/query.rs",
     """        query_type: word(20),
        query_count: word(24),""",
     """        query_type: word(24),
        query_count: word(20),""",
     VULKAN),
    ("vkquery-B1", "B", "a destroyed query pool stays registered", "crates/omni-android/src/vulkan/query.rs",
     """    vulkan.forget_query_pool(handle);""",
     """    let _ = handle;""",
     VULKAN),
    # AAsset_openFileDescriptor: -1 is the device's answer for a compressed asset; a stored one
    # answered -1 too would hide a descriptor the engine was owed.
    ("assetfd-B1", "B", "a stored asset is answered -1 as if compressed", "crates/omni-android/src/ndk/assets.rs",
     """        Some(AssetPlacement::NotInAnyFile) => {
            let mut state = ndk.state.lock();""",
     """        Some(AssetPlacement::NotInAnyFile | AssetPlacement::StoredInPackage { .. }) => {
            let mut state = ndk.state.lock();""",
     ANDROID),
    # Descriptor update templates: a stride ignored reads every descriptor from the first one's
    # place; a destroyed template left registered updates through entries that are gone.
    ("vktemplate-A1", "A", "a template's stride is ignored", "crates/omni-android/src/vulkan/descriptor.rs",
     """            let offset = entry.stride.checked_mul(element).and_then(|step| step.checked_add(entry.offset));""",
     """            let offset = entry.stride.checked_mul(0).and_then(|step| step.checked_add(entry.offset + 0 * element));""",
     VULKAN),
    ("vktemplate-B1", "B", "a destroyed template stays registered", "crates/omni-android/src/vulkan/descriptor.rs",
     """    vulkan.forget_update_template(handle);""",
     """    let _ = handle;""",
     VULKAN),
    # erfcf: one coefficient transposed is a function that is right at every exact special value
    # and wrong everywhere in between.
    ("erfcf-A1", "A", "erfcf's first interval loses a coefficient", LIBM,
     """        let r = pp0 + z * (pp1 + z * pp2);
        let s = one + z * (qq1 + z * (qq2 + z * qq3));
        let y = r / s;
        if hx < 0x3e80_0000 {""",
     """        let r = pp0 + z * (pp1 + z * pp1);
        let s = one + z * (qq1 + z * (qq2 + z * qq3));
        let y = r / s;
        if hx < 0x3e80_0000 {""",
     BIONIC_LIBM),
    # The timer's commands: the stage and the pool swapped put a handle where a bit mask goes.
    ("vkquery-A2", "A", "vkCmdWriteTimestamp's stage and query are swapped", "crates/omni-android/src/vulkan/query.rs",
     """    host.cmd_write_timestamp(buffer, args[1] as u32, pool, args[3] as u32)?;""",
     """    host.cmd_write_timestamp(buffer, args[3] as u32, pool, args[1] as u32)?;""",
     VULKAN),
    # Integer widths in printf: an unmodified %u read whole prints the caller's garbage; a %ld
    # narrowed to 32 bits prints a different number. Both look like numbers.
    ("printfw-A1", "A", "an unmodified %u takes the whole slot", BIONIC_PRINTF,
     """        "hh" => raw as u8 as u64,
        "h" => raw as u16 as u64,
        "" => raw as u32 as u64,""",
     """        "hh" => raw as u8 as u64,
        "h" => raw as u16 as u64,""",
     BIONIC),
    ("printfw-A2", "A", "%ld is narrowed to 32 bits", "crates/omni-android/src/bionic/format.rs",
     """            ArgKind::Int => Owned::Int(source.next_u64()? as i64),""",
     """            ArgKind::Int => Owned::Int(source.next_u64()? as u32 as i32 as i64),""",
     ANDROID),
    # The display: a density of zero is a number the engine divides by, not a refusal.
    ("dispmetrics-A1", "A", "DisplayMetrics.density answers 0 again", JNI_CLASSES,
     """            f("density", "F", Answer::Unanswered),""",
     """            f("density", "F", Answer::Float(0.0)),""",
     ANDROID_LIB),
    # Compute pipelines: the stage is embedded, so every member after it is at a fixed offset a
    # transposition moves silently; and a stage bit the driver is never checked against.
    ("vkcompute-A1", "A", "a compute pipeline's layout is read at the base pipeline's offset",
     "crates/omni-android/src/vulkan/shader.rs",
     """        let layout = vulkan.pipeline_layout_token(at, CALL, u64_at(72))?;""",
     """        let layout = vulkan.pipeline_layout_token(at, CALL, u64_at(80))?;""",
     VULKAN),
    ("vkcompute-A2", "A", "a compute pipeline's stage bit is not checked",
     "crates/omni-android/src/vulkan/shader.rs",
     """        if stage.stage != SHADER_STAGE_COMPUTE {""",
     """        if false && stage.stage != SHADER_STAGE_COMPUTE {""",
     VULKAN),
    ("vkcompute-A3", "A", "basePipelineIndex is read from the padding after it",
     "crates/omni-android/src/vulkan/shader.rs",
     """            base_pipeline_index: u32_at(88) as i32,""",
     """            base_pipeline_index: u32_at(92) as i32,""",
     VULKAN),
    ("vkdispatch-A1", "A", "vkCmdDispatch's y and z group counts are swapped",
     "crates/omni-android/src/vulkan/draw.rs",
     """    host.cmd_dispatch(buffer, args[1] as u32, args[2] as u32, args[3] as u32)?;""",
     """    host.cmd_dispatch(buffer, args[1] as u32, args[3] as u32, args[2] as u32)?;""",
     VULKAN),
    ("vkcopyimage-A1", "A", "vkCmdCopyImage's two layouts are swapped",
     "crates/omni-android/src/vulkan/draw.rs",
     """    host.cmd_copy_image(buffer, source, args[2] as u32, destination, args[4] as u32, &regions)?;""",
     """    host.cmd_copy_image(buffer, source, args[4] as u32, destination, args[2] as u32, &regions)?;""",
     VULKAN),
    ("vkcopyimage-A2", "A", "vkCmdCopyImage's two images are swapped",
     "crates/omni-android/src/vulkan/draw.rs",
     """    let destination = vulkan.image_ref_token(at, CALL, args[3])?;
    let regions =
        read_regions(c, at, CALL, "pRegions", "VkImageCopy",""",
     """    let (source, destination) = (vulkan.image_ref_token(at, CALL, args[3])?, source);
    let regions =
        read_regions(c, at, CALL, "pRegions", "VkImageCopy",""",
     VULKAN),
    ("vkblit-A1", "A", "vkCmdBlitImage's filter in x7 is dropped for NEAREST",
     "crates/omni-android/src/vulkan/draw.rs",
     """        &regions,
        args[7] as u32,
    )?;""",
     """        &regions,
        0,
    )?;""",
     VULKAN),
    ("vkbarriers-A1", "A", "the barrier bound goes back to the 32 the engine's 135 exceed",
     "crates/omni-android/src/vulkan/command.rs",
     """pub const MAX_BARRIERS: usize = super::MAX_CREATED_IMAGES / 2;""",
     """pub const MAX_BARRIERS: usize = 32;""",
     VULKAN),
    # Query results without WAIT: the unavailable ones keep the guest's bytes, and a short buffer
    # is a driver write past the host's.
    ("vkqueryres-A1", "A", "the host writes into zeros instead of the guest's own bytes",
     "crates/omni-android/src/vulkan/query.rs",
     """    let mut data = c.mem().read_bytes(data_at, span, c.blame(5))?;""",
     """    let mut data = vec![0u8; span];""",
     VULKAN),
    ("vkqueryres-A2", "A", "a dataSize short of the span is not refused",
     "crates/omni-android/src/vulkan/query.rs",
     """    if data_size < span as u64 {""",
     """    if false && data_size < span as u64 {""",
     VULKAN),
    ("vkqueryres-B1", "B", "VK_NOT_READY is treated as a failure and nothing is written back",
     "crates/omni-android/src/vulkan/query.rs",
     """    if result >= 0 {""",
     """    if result == VK_SUCCESS {""",
     VULKAN),
    # Stand-ins for the characters Windows reserves: stored, listed back, and one-to-one.
    ("standin-A1", "A", "a component reaches the host with its reserved characters as themselves",
     "crates/omni-platform/src/fs/path.rs",
     """        host.push(host_component(component));""",
     """        host.push(component);""",
     PLATFORM),
    ("standin-A2", "A", "a listed name keeps its stand-ins instead of the guest's characters",
     PLAT_FS,
     """            let name = path::guest_component(host_name);""",
     """            let name = host_name.to_string();""",
     PLATFORM),
    ("standin-B1", "B", "a stand-in the guest writes itself is accepted, aliasing its character",
     "crates/omni-platform/src/fs/path.rs",
     """    if let Some(bad) = name.chars().find(|&c| STORED_AS_STAND_IN.iter().any(|&r| stand_in(r) == c)) {""",
     """    if let Some(bad) = name.chars().find(|&c| false && STORED_AS_STAND_IN.iter().any(|&r| stand_in(r) == c)) {""",
     PLATFORM),
    ("standin-B2", "B", "a device stem ends only at a dot, so `NUL:` is stored as a device name",
     "crates/omni-platform/src/fs/path.rs",
     """        .split(|c: char| c == '.' || STORED_AS_STAND_IN.contains(&c))""",
     """        .split(|c: char| c == '.')""",
     PLATFORM),
    # scanf: the return rule, the prefixes, the scanset, and where each value is written.
    ("scanf-A1", "A", "an input failure after a conversion answers EOF instead of the count",
     "crates/omni-bionic/src/scanf.rs",
     """        result: if conversions == 0 { EOF } else { assigned },""",
     """        result: EOF,""",
     SCANF),
    ("scanf-A2", "A", "a negated scanset is read as a plain one",
     "crates/omni-bionic/src/scanf.rs",
     """    let negate = format.get(f) == Some(&b'^');""",
     """    let negate = false && format.get(f) == Some(&b'^');""",
     SCANF),
    ("scanf-A3", "A", "%lld is stored in four bytes",
     "crates/omni-bionic/src/scanf.rs",
     """            Length::Long | Length::Wide => 8,""",
     """            Length::Long => 8,
            Length::Wide => 4,""",
     SCANF),
    ("scanf-B1", "B", "%n is counted as an assignment",
     "crates/omni-bionic/src/scanf.rs",
     """                stores.push(Store::Int { value: at as u64, size: length.int_size() });""",
     """                stores.push(Store::Int { value: at as u64, size: length.int_size() });
                assigned += 1;""",
     SCANF),
    ("scanf-B2", "B", "a bare 0x keeps its x instead of giving it back",
     "crates/omni-bionic/src/scanf.rs",
     """    if matches!(field.last(), Some(b'x' | b'X')) {""",
     """    if false && matches!(field.last(), Some(b'x' | b'X')) {""",
     SCANF),
    ("sscanf-A1", "A", "a scanned string is written without its NUL",
     "crates/omni-android/src/bionic/format.rs",
     """                terminated.push(0);""",
     """                let _ = &mut terminated;""",
     ANDROID),
    # sem_*: the family's -1-with-errno convention, and the value the word starts at.
    ("sem-A1", "A", "a sem_* failure returns -1 with the guest's errno left stale",
     "crates/omni-android/src/bionic/handlers.rs",
     """        view.set_errno(omni_bionic::sem::last_errno());""",
     """        let _ = omni_bionic::sem::last_errno();""",
     ANDROID),
    ("sem-A2", "A", "sem_init ignores the value it is given",
     "crates/omni-android/src/bionic/handlers.rs",
     """        let produced = omni_bionic::sem::init(&mut view, sem, pshared, value);""",
     """        let produced = omni_bionic::sem::init(&mut view, sem, pshared, value & 0);""",
     ANDROID),
    # setpriority: applied to the host thread, by the stated table, and only for the caller.
    ("prio-A1", "A", "setpriority answers 0 without applying the nice value",
     "crates/omni-android/src/bionic/procenv.rs",
     """    if let Err(error) = omni_platform::process::set_current_thread_nice(nice) {""",
     """    if let Err(error) = Ok::<(), omni_platform::process::ProcessError>(()) {""",
     ANDROID),
    ("prio-A2", "A", "Android's AUDIO nice (-16) lands a tier low",
     "crates/omni-platform/src/process/windows.rs",
     """        i32::MIN..=-11 => THREAD_PRIORITY_HIGHEST,
        -10..=-1 => THREAD_PRIORITY_ABOVE_NORMAL,""",
     """        i32::MIN..=-17 => THREAD_PRIORITY_HIGHEST,
        -16..=-1 => THREAD_PRIORITY_ABOVE_NORMAL,""",
     PLATFORM),
    ("prio-B1", "B", "setpriority applies to whatever thread `who` names",
     "crates/omni-android/src/bionic/procenv.rs",
     """    let is_me = who == 0 || me.is_some_and(|thread| u64::from(who) == thread.0);""",
     """    let is_me = true || who == 0 || me.is_some_and(|thread| u64::from(who) == thread.0);""",
     ANDROID),
    # JNIEnv slots: returned when a guest thread ends, and the lowest free one handed out.
    ("jnislot-A1", "A", "a guest thread's JNIEnv slot is never returned (the 65th thread is refused)",
     "crates/omni-android/src/jni/mod.rs",
     """        self.jni.release_thread(self.thread);""",
     """        let _ = self.thread;""",
     ANDROID_LIB),
    ("jnislot-A2", "A", "a new thread takes the live-thread count as its slot, sharing a live env",
     "crates/omni-android/src/jni/mod.rs",
     """                    let Some(next) = (0..MAX_JNI_THREADS).find(|index| !taken.contains(index)) else {""",
     """                    let Some(next) = Some(taken.len()).filter(|n| *n < MAX_JNI_THREADS) else {""",
     ANDROID_LIB),
    ("jnidebug-A1", "A", "a Java debugger is reported connected to a runtime with no JDWP",
     JNI_CLASSES,
     """        methods: &[s("isDebuggerConnected", "()Z", Answer::Bool(false))],""",
     """        methods: &[s("isDebuggerConnected", "()Z", Answer::Bool(true))],""",
     ANDROID_LIB),
    ("vkcompute-B1", "B", "a NULL base pipeline is resolved as a handle",
     "crates/omni-android/src/vulkan/shader.rs",
     """            if u64_at(80) == 0 { None } else { Some(vulkan.pipeline_token(at, CALL, u64_at(80))?) };""",
     """            if false { None } else { Some(vulkan.pipeline_token(at, CALL, u64_at(80))?) };""",
     VULKAN),
    ("deathctx-A3", "A", "the stack snapshot starts one word above SP", ADAPTER_THREADS,
     """    for at in (sp..sp.saturating_add(DEATH_STACK_BYTES)).step_by(8) {""",
     """    for at in (sp + 8..sp.saturating_add(DEATH_STACK_BYTES)).step_by(8) {""",
     ANDROID),

    # A thread that exits without giving its block back leaks one per thread, so a guest that
    # creates and joins in a loop stops being able to create threads after 64 of them -- with a
    # refusal that names the thread count and points at the guest rather than at this line.
    ("threads-A7", "A", "an exited guest thread never gives its arena block back",
     ADAPTER_THREADS,
     """    let _ = bionic.threads_table().detach_current();""",
     """    let _ = ();""",
     ANDROID),

    # The over-correction on `pthread_detach`: treating the FIRST detach as the one that is not
    # joinable. A guest that detaches once and never joins then leaks a record for every thread.
    ("threads-B1", "B", "the first pthread_detach is refused as well as the second", ADAPTER_MOD,
     """        if record.detached {
            // POSIX: "the value specified by thread does not refer to a joinable thread". A""",
     """        if !record.detached {
            // POSIX: "the value specified by thread does not refer to a joinable thread". A""",
     ANDROID),

    # The over-correction on `pthread_getschedparam`: refusing it along with the signal family,
    # on a "this runtime does not model scheduling" argument. Nothing in the reachable 188 can
    # SET a policy, so the default is forced rather than approximated, and refusing it stops a
    # correct guest over a field it is only reading.
    ("threads-B2", "B", "pthread_getschedparam refuses instead of answering the forced default",
     ADAPTER_THREADS,
     """    if !known {
        c.ret().i32(consts::ESRCH);
        return Ok(());
    }""",
     """    if !known || thread != u64::MAX {
        return call.refuse("this runtime does not model scheduling policy");
    }""",
     ANDROID),

    # ------------------------------------------------ phase 3c: cross-context code invalidation

    # The state phase 2 left: `invalidate_code` reaching ONE context, so a second guest thread
    # that had translated the same range keeps executing bytes that are no longer mapped.
    ("watch-A1", "A", "an unmap reaches only the calling thread's context", BOUNDARY,
     """        let me = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(token, _)| *token));
        self.boundary.code_watch.broadcast(me.unwrap_or(u64::MAX), (address, len));""",
     """        let me = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(token, _)| *token));
        let _ = (me, address, len);""",
     ANDROID),

    # The over-correction: collapsing to the whole address space on the first range rather than
    # on a full queue. It is always *safe* -- over-invalidating costs translation and nothing
    # else -- which is exactly why nothing but a counter can see it.
    ("watch-B1", "B", "every cross-context invalidation collapses to the whole address space",
     BOUNDARY,
     """            if inner.ranges.len() >= MAX_PENDING_INVALIDATIONS {""",
     """            if inner.ranges.len() < MAX_PENDING_INVALIDATIONS {""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: the arena's bases

    # The accessor/layout disagreement an independent review found: the arena test restated
    # `ARENA_BYTES`'s own definition, which cannot fail, so nothing checked that the four
    # accessors agreed with it. With this applied, `fopen` hands out `FILE` objects on top of the
    # pool's interned strings.
    ("arena-A1", "A", "the FILE table starts on top of the pool", ADAPTER_MOD,
     """    pub fn files_base(&self) -> GuestAddr {
        self.pool() + POOL_BYTES
    }""",
     """    pub fn files_base(&self) -> GuestAddr {
        self.pool()
    }""",
     ANDROID_LIB),

    # ---- the gmtime wrap: a defect only a DEBUG build can see -----------------------------------
    # `days * SECONDS_PER_DAY` exceeds i64 only near i64::MIN, and every such timestamp is in a year
    # around -2.9e11, which `i32::try_from(year - 1900)` refuses whatever `second_of_day` holds. So
    # in release the product wraps, the garbage is discarded by that refusal, and the behaviour is
    # correct BY ACCIDENT -- which is why the whole workspace suite stayed green while the Critical
    # was live, and why its regression test could assert nothing and still look like a guard.
    # This row is the detector: `mutate.py` runs debug, where the multiplication panics.
    ("time-A7", "A",
     "the day remainder goes back to a subtraction that overflows near i64::MIN",
     BIONIC_TIME,
     """    let second_of_day = timestamp.rem_euclid(SECONDS_PER_DAY);""",
     """    let second_of_day = timestamp - floor_div(timestamp, SECONDS_PER_DAY) * SECONDS_PER_DAY;""",
     BIONIC),

    # ---- the confinement's last two rules, which had no row at all ------------------------------
    # A review found rules 5 and 6 covered by exactly one test, which SILENTLY SKIPPED on this host:
    # an unelevated Windows session cannot create a symbolic link (MEASURED: WinError 1314). So on
    # the machine whose green suite was the evidence for them, neither rule had ever executed, and
    # fs-A1..A4/B1..B3 all sit in rules 1-4. The test now falls back to a directory junction, which
    # needs no privilege, and FAILS LOUDLY rather than skipping if it can make neither.
    ("fs-A7", "A",
     "the symlink refusal never fires, so a link inside the root is followed out of it",
     PLAT_FS_PATH,
     """            Ok(metadata) if metadata.file_type().is_symlink() => {""",
     """            Ok(metadata) if false && metadata.file_type().is_symlink() => {""",
     PLATFORM),

    # Rule 6 guards `PathBuf::push` with an ABSOLUTE component, which replaces the path rather than
    # appending. `starts_with` is lexical, so a `..` component would never reach this -- which is
    # why the test builds its `Resolved` by hand.
    ("fs-A8", "A",
     "the containment check never fires, so an absolute component escapes the root",
     PLAT_FS_PATH,
     """    if !host.starts_with(root) {""",
     """    if false && !host.starts_with(root) {""",
     PLATFORM),

    # ================================================================ phase 3d: the network group
    #
    # `omni-platform` did not grow for this phase, so there is nothing to mutate on that side.
    # `poll` and `select` are mutated where their answer is decided, which is the adapter, and
    # `inet_ntop`'s formatting is mutated in `omni-bionic`, which is where BIND's rules live.

    # BIND formats into a local buffer and only then compares against `size`. Without the compare,
    # a destination the guest said was eight bytes long receives nine -- and the nine are a
    # truncated address, which is still a printable string naming a different host.
    ("net-A1", "A", "inet_ntop writes a truncated address instead of reporting ENOSPC",
     BIONIC_NET,
     """    if size as usize <= text.len() {""",
     """    if false && size as usize <= text.len() {""",
     BIONIC),

    # `best.len > 1`. A naive "compress the longest run" produces `1::2:3:4:5:6:7` for an address
    # with one zero group: a different, shorter, plausible spelling, and not bionic's.
    ("net-A2", "A", "a single zero group is compressed, which is not what BIND does",
     BIONIC_NET,
     """    let best = best.filter(|(_, len)| *len > 1);""",
     """    let best = best.filter(|(_, len)| *len > 0);""",
     BIONIC),

    # The encapsulated-IPv4 tail. Without `len == 6`, `::1.2.3.4` prints as `::102:304` -- the same
    # address, spelled the way the modern standard library spells it and not the way bionic does.
    # The 200,000-address differential run found that divergence rather than assuming it, and this
    # row is what keeps the finding.
    ("net-A3", "A", "the IPv4-compatible form loses its dotted tail",
     BIONIC_NET,
     """                base == 0 && (len == 6 || (len == 5 && words[5] == 0xFFFF))""",
     """                base == 0 && (len == 5 && words[5] == 0xFFFF)""",
     BIONIC),

    # The pooled `gai_strerror` table. A bound of one leaves every code but `Success` falling
    # through to "Unknown error", which is a string, prints, and says nothing.
    ("net-A4", "A", "gai_strerror answers Unknown error for codes that are in its table",
     ADAPTER_MOD,
     """            Ok(index) if index < known => index,""",
     """            Ok(index) if index < 1 => index,""",
     ANDROID),

    # The fallback row is interned last and `gai_message` indexes it by `known`. Without it, a code
    # outside the table indexes past the end and gets the pool's base, which holds `"UTC"`.
    ("net-A5", "A", "the gai_strerror fallback row is never interned",
     ADAPTER_MOD,
     """        for code in 0..=omni_bionic::net::GAI_MESSAGES as i32 {""",
     """        for code in 0..omni_bionic::net::GAI_MESSAGES as i32 {""",
     ANDROID),

    # POSIX: a negative descriptor is ignored with a zeroed `revents`. It is the idiom for a slot a
    # program has stopped using, so `POLLNVAL` there makes every such program see an error it has
    # no cause for -- and makes `poll` return a non-zero count for an array of disabled slots.
    ("net-A6", "A", "a negative pollfd is reported POLLNVAL instead of being ignored",
     ADAPTER_NET,
     """        let revents = if fd < 0 {""",
     """        let revents = if false && fd < 0 {""",
     ANDROID),

    # A descriptor nothing opened reported as ready. The guest then reads it and gets EBADF from a
    # call `poll` had just said would not block.
    # **RE-ANCHORED in M5**, when `poll` stopped answering "open, therefore ready" and started
    # asking `Filesystem::readiness`. The property is unchanged and so is the detector; what moved
    # is the line that holds it. The pre-flight is what found the staleness, which is what it is
    # for — six rows went stale the same way once before.
    ("net-A7", "A", "a descriptor that is not open is reported ready rather than POLLNVAL",
     ADAPTER_NET,
     """                _ => POLLNVAL,""",
     """                _ => events & READY_MASK,""",
     ANDROID),

    # **Review finding M1's shape, in this group.** Read the whole array, decide, write it once --
    # against reading and writing entry by entry, which leaves a half-updated `revents` array
    # behind a reported failure. One row rather than two, because each half alone is invisible: a
    # per-entry read fails before the whole-array write is reached, and a per-entry write is never
    # reached after a whole-array read has failed. VERIFIED as a detector before the row was
    # written -- the first entry's sentinel is overwritten and
    # `a_poll_array_that_runs_off_its_mapping_leaves_the_first_entry_untouched` fails.
    ("net-A8", "A", "poll answers the guest's array entry by entry instead of all at once",
     ADAPTER_NET,
     """    let mut entries = view.mem().read_bytes(at, bytes, blame)?;""",
     """    let mut entries = vec![0u8; bytes];
    for (i, chunk) in entries.chunks_exact_mut(POLLFD_BYTES).enumerate() {
        chunk.copy_from_slice(&view.mem().read_bytes(at + i * POLLFD_BYTES, POLLFD_BYTES, blame)?);
        let mut answer = [chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], 1, 0];
        if i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) < 0 {
            answer[6] = 0;
        }
        view.mem().write_bytes(at + i * POLLFD_BYTES, &answer, blame)?;
    }""",
     ANDROID),

    # `nfds` is an `nfds_t`, which is 64 bits and is the guest's. Without the cap, `poll(p, 1025,
    # 0)` reads an array eight kilobytes long out of whatever follows the guest's own, and
    # `poll(p, SIZE_MAX, 0)` asks this layer for 147 exabytes -- which in a debug build is an
    # arithmetic panic reachable from guest input, the shape Global Constraint 11 calls Critical.
    ("net-A10", "A", "poll's nfds cap removed, so a guest-chosen count is honoured",
     ADAPTER_NET,
     """        if nfds > MAX_POLL_FDS {""",
     """        if false && nfds > MAX_POLL_FDS {""",
     ANDROID),

    # `select` reports `EBADF` for the CALL, not for one bit. Without the check, a descriptor
    # nothing opened is reported ready in whichever set named it.
    ("net-A11", "A", "select reports a descriptor nothing opened as ready",
     ADAPTER_NET,
     """        if named.iter().any(|fd| !fs.is_open(*fd)) {""",
     """        if named.iter().any(|fd| !fs.is_open(*fd)) && false {""",
     ANDROID),

    # POSIX: `select` returns the total number of bits set across all the masks, so one descriptor
    # ready in two sets is two. Counting descriptors instead returns one -- a smaller, entirely
    # reasonable-looking number.
    # **RE-ANCHORED in M5.** `sets[2].clear()` moved into `answer_sets` and the count moved into
    # `ready_bits`, which the first evaluation and every pass of the wait now share — so the row
    # anchors on the function rather than on one of two identical expressions.
    ("net-A12", "A", "select counts ready descriptors instead of ready bits",
     ADAPTER_NET,
     """    sets[0].count(nfds) + sets[1].count(nfds)""",
     """    sets[0].count(nfds).max(sets[1].count(nfds))""",
     ANDROID),

    # Nothing in this runtime can raise an exception condition, so the exception set comes back
    # empty. Leaving the guest's own bits in it says every descriptor it asked about has one.
    # **RE-ANCHORED in M5**: the clear moved into `answer_sets`, which is now the one place any
    # set is answered. **RE-ANCHORED AGAIN in M6, and the second time is the lesson**
    # (VERIFICATION entry 8): `answer_sets` grew a `Watch` return for the mixed socket/pipe wait,
    # so `sets[2].clear();` stopped being the last line of the function and this row silently
    # matched nothing. `--only sock` would never have seen it; the whole-table pre-flight did.
    ("net-A13", "A", "select leaves the guest's bits in the exception set",
     ADAPTER_NET,
     """    sets[2].clear();

    let mut watch = Watch::default();""",
     """    let mut watch = Watch::default();""",
     ANDROID),

    # Bionic converts the `timeval` before the syscall and reports a `tv_usec` outside [0, 1e6) as
    # EINVAL itself. Without the check, `{i64::MIN, i64::MIN}` reaches the duration arithmetic.
    ("net-A14", "A", "select accepts a malformed struct timeval",
     ADAPTER_NET,
     """        if !(0..MICROS_PER_SECOND).contains(&micros) || seconds < 0 {""",
     """        if false && (!(0..MICROS_PER_SECOND).contains(&micros) || seconds < 0) {""",
     ANDROID),

    # `socket` answering -1/EAFNOSUPPORT is the most believable wrong answer this phase had: a
    # legitimate POSIX outcome that a networked program branches on quietly, so the engine disables
    # its own networking during initialisation and nothing records that this layer, rather than the
    # device, decided that.
    ("net-A15", "A", "socket answers -1/EAFNOSUPPORT instead of refusing",
     ADAPTER_NET,
     """    let family = match domain {""",
     """    if domain != i32::MIN {
        let state = active(c.symbol(), c.address())?;
        {
            let mut view = enter(c, &state);
            view.set_errno(consts::EAFNOSUPPORT);
        }
        c.ret().i32(-1);
        return Ok(());
    }
    let family = match domain {""",
     ANDROID),

    # `freeaddrinfo` returns `void`, which is what makes a silent no-op the dangerous answer: there
    # is no value to be wrong, so nothing distinguishes it from a correct free.
    ("net-A16", "A", "freeaddrinfo quietly does nothing instead of refusing",
     ADAPTER_NET,
     """    let res = c.args().next_u64()?;""",
     """    let res = c.args().next_u64()?;
    if res != u64::MAX {
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),


    # POSIX: "on failure, the objects pointed to by the readfds, writefds, and errorfds arguments
    # are not modified". With the sets rewritten before the timeout is read, a `tv_usec` of
    # 1,000,000 answers -1/EINVAL **and takes the guest's sets with it**, so a caller that retried
    # the call would retry it with nothing. This was a real defect in the first version of this
    # module, found by re-reading the code rather than by a failing test; `net-A9` is the row that
    # keeps it found.
    # **RE-ANCHORED in M5**, when the sleep became a wait that re-asks the question. The mutation
    # is the same one — zero and write back the guest's sets *before* the timeout is validated —
    # expressed against the new shape: the `timeout == 0` branch is where the parse begins, so
    # clearing and writing there puts the whole rest of the validation after the damage.
    ("net-A9", "A", "select zeroes the guest's sets before it validates the timeout",
     ADAPTER_NET,
     """    let duration = if timeout == 0 {""",
     """    for set in &mut sets {
        set.clear();
    }
    for set in &sets {
        set.write_back(view)?;
    }
    let duration = if timeout == 0 {""",
     ANDROID),

    # ---- the over-corrections ------------------------------------------------------------------

    # The cap on a wait applied to every wait, so a `poll` with a thirty-millisecond timeout is
    # refused. A guest polling with a short timeout is the ordinary case, and refusing it stops a
    # correct guest over a bound that exists for a hostile one.
    # **Re-anchored in M6's network phase**, when the cap stopped being unconditional: a set that
    # can become ready now waits as long as the guest asked (see `Watch::can_change` and
    # `sockcfg-B2`). The row's meaning is unchanged -- it refuses every bounded wait -- and the
    # `!can_change &&` is dropped along with the comparison, which is what makes it do that.
    ("net-B1", "B", "every bounded wait is refused, not only one past the cap",
     ADAPTER_NET,
     """    if !can_change && duration.as_secs() > MAX_SLEEP_SECONDS {""",
     """    if duration.as_millis() > 0 {""",
     ANDROID),

    # `nfds == FD_SETSIZE` is the last legal value: an `fd_set` holds descriptors 0..FD_SETSIZE, so
    # `select(FD_SETSIZE, ..)` names all of them. Excluding it refuses a correct call.
    ("net-B2", "B", "select refuses an nfds of exactly FD_SETSIZE",
     ADAPTER_NET,
     """    if !(0..=FD_SETSIZE).contains(&nfds) {""",
     """    if !(0..FD_SETSIZE).contains(&nfds) {""",
     ANDROID),

    # The descriptor table consulted whether or not any entry names a descriptor, so a `poll` over
    # an array of disabled slots refuses on an instance with no filesystem root. `poll` needs a
    # descriptor table only when it is asked about a descriptor.
    ("net-B3", "B", "poll needs a filesystem even when it is asked about no descriptors",
     ADAPTER_NET,
     """    let fs = if names_a_descriptor { Some(filesystem(view)?) } else { None };""",
     """    let _ = names_a_descriptor;
    let fs = Some(filesystem(view)?);""",
     ANDROID),

    # ---- M6: `inet_pton`, the other direction ---------------------------------------------------
    # Found by a guest worker thread dying on it (thread 7, start routine at image offset
    # 0x2217f04), and the only symbol added this session that needed new computation rather than a
    # binding. Every row below removes ONE strictness rule, because `inet_pton`'s whole reason to
    # exist beside `inet_aton` is that it has them: each mutation turns a text the function must
    # reject into a valid-looking address for a DIFFERENT host, which is the shape of the parser
    # CVEs -- two parsers reading one string and disagreeing about what it names. None of them is
    # a crash and none of them is visible in the return value alone.
    #
    # The parsing is mutated in `omni-bionic`, where BIND's rules live, and the one adapter row is
    # about errno, which is the adapter's to get wrong.

    # **Leading zeros.** `inet_aton` reads `010.1.1.1` as octal and names 8.1.1.1; a decimal reader
    # names 10.1.1.1. BIND's `saw_digit && *tp == 0` refuses the spelling outright, which is the
    # only fix that does not depend on which reading the other parser chose.
    ("pton-A1", "A", "a leading zero is accepted, so 010.1.1.1 parses as some host or other",
     BIONIC_NET,
     """            if saw_digit && tmp[octet] == 0 {
                return Ok(None);
            }""",
     """            if false && saw_digit && tmp[octet] == 0 {
                return Ok(None);
            }""",
     BIONIC),

    # **Two `::`.** How many zero groups each one stands for is undecidable, so there is no address
    # to return -- but a parser that just skips empty groups returns one, and it is an address the
    # guest never wrote down.
    ("pton-A2", "A", "a second :: is accepted, so an ambiguous address parses as one of its readings",
     BIONIC_NET,
     """                if colonp.is_some() {
                    return Ok(None);
                }""",
     """                if false && colonp.is_some() {
                    return Ok(None);
                }""",
     BIONIC),

    # **`dst` on a failed parse.** BIND accumulates into a local and only a complete address is
    # copied out; handing the partial `tmp` back instead writes whatever had been parsed so far
    # into the guest's `in6_addr` AND reports success. A caller that ignores the return value --
    # and the ones that do not, here -- reads a different address with no sign of where it came
    # from. This is the row that fails if the "write nothing" property stops being asserted.
    ("pton-A3", "A", "a refused parse hands back the bytes it had, so dst receives a partial address",
     BIONIC_NET,
     """    // Refusal 7.
    if tp != endp {
        return Ok(None);
    }""",
     """    // Refusal 7.
    if tp != endp {
        return Ok(Some(tmp));
    }""",
     BIONIC),

    # **A leading `:` that is not part of `::`.** `:1:2:3:4:5:6:7:8` has eight groups and reads as
    # a perfectly ordinary address once the stray colon is swallowed.
    ("pton-A4", "A", "a stray leading colon is swallowed instead of refused",
     BIONIC_NET,
     """        if scan.at(1)? != Ch::Byte(b':') {
            return Ok(None);
        }""",
     """        if false {
            return Ok(None);
        }""",
     BIONIC),

    # **A `::` that stands for no groups at all.** `1:2:3:4:5:6:7:8::` is already sixteen bytes, so
    # the `::` compresses nothing -- and `::` is defined to stand for one group or more. Without
    # `tp == endp` it parses as the eight groups with the colons ignored.
    ("pton-A5", "A", "a :: in an already-full address is ignored rather than refused",
     BIONIC_NET,
     """        if tp == endp {
            return Ok(None);
        }""",
     """        if false && tp == endp {
            return Ok(None);
        }""",
     BIONIC),

    # **A fifth hex digit.** `12345::` would take only the low sixteen bits and parse as `2345::`,
    # which is a real address and the wrong one -- the silent-truncation form of this defect.
    ("pton-A6", "A", "a fifth hex digit is taken, so a group truncates to its low sixteen bits",
     BIONIC_NET,
     """            if seen_xdigits > 4 {
                return Ok(None);
            }""",
     """            if seen_xdigits > 5 {
                return Ok(None);
            }""",
     BIONIC),

    # **A trailing `:`.** BIND looks one character ahead at each separator precisely for this, and
    # without it `1:2:3:4:5:6:7:8:` and `1::2:` both parse.
    ("pton-A7", "A", "a trailing colon separates nothing and is accepted anyway",
     BIONIC_NET,
     """            if scan.at(i)? == Ch::End {
                return Ok(None);
            }""",
     """            if false {
                return Ok(None);
            }""",
     BIONIC),

    # **Shorthand.** `inet_aton("127.1")` is 127.0.0.1 and `inet_aton("127")` is 0.0.0.127;
    # `inet_pton` takes four octets or nothing. Without `octets < 4` the missing ones are the zeros
    # the local array started as, so `127.1` parses -- as 127.1.0.0, which is neither reading.
    ("pton-A8", "A", "fewer than four octets is accepted, so 127.1 parses as 127.1.0.0",
     BIONIC_NET,
     """    if octets < 4 {
        return Ok(None);
    }
    Ok(Some(tmp))""",
     """    if false && octets < 4 {
        return Ok(None);
    }
    Ok(Some(tmp))""",
     BIONIC),

    # **A 0 is a parse answer, not an error.** Setting errno for it is the believable wrong answer:
    # it reads as diligence, and it breaks the standard "try AF_INET, then AF_INET6" routine, whose
    # second call has to see the errno from the attempt that was MEANT to fail.
    ("pton-A9", "A", "a 0 from inet_pton sets errno, so the family-probing idiom reads the wrong one",
     ADAPTER_NET,
     """            Ok(Ok(false)) => 0,""",
     """            Ok(Ok(false)) => {
                view.set_errno(consts::EINVAL);
                0
            }""",
     ANDROID),

    # The over-correction, and it is the one a careful reader makes: a zero octet looks like the
    # leading zero the rule above refuses, so the check fires on the first digit rather than on a
    # digit that FOLLOWS a zero -- and 0.0.0.0 and 10.0.0.1 stop being addresses.
    ("pton-B1", "B", "every zero octet is read as a leading zero, so 10.0.0.1 is refused",
     BIONIC_NET,
     """            if saw_digit && tmp[octet] == 0 {""",
     """            if tmp[octet] == 0 {""",
     BIONIC),

    # The second over-correction, and it is `inet_ntop`'s own rule in the wrong direction: BIND
    # will not PRINT a `::` that stands for a single zero group (`best.len > 1`), which is a
    # formatting rule and says nothing about what may be READ. `1:2:3:4:5:6::8` is legal input.
    ("pton-B2", "B", ":: is required to stand for two groups, which is a printing rule not a parsing one",
     BIONIC_NET,
     """        if tp == endp {
            return Ok(None);
        }
        // Slide everything after the run to the end""",
     """        if tp + 2 >= endp {
            return Ok(None);
        }
        // Slide everything after the run to the end""",
     BIONIC),

    # ================================================================ phase 3e: the last six

    # The two `__gcov_*` imports resolve to nothing only because the reference is WEAK. Without
    # that check every declared-absent symbol resolves to nothing however it is referenced, and a
    # strong reference becomes a branch to address zero with no symbol attached -- the failure the
    # whole thunk region exists to replace.
    ("gcov-B1", "B", "a strong reference to an absent symbol also resolves to nothing",
     BOUNDARY,
     """                if request.weak && self.is_absent(request.name) {""",
     """                if self.is_absent(request.name) {""",
     ANDROID),

    # And the revert: the absent list ignored, so `__gcov_dump` gets a thunk address, the guest's
    # own `CBZ` falls through, and it calls a symbol no Android device supplies -- followed, four
    # bytes later, by `BL abort`.
    ("gcov-A1", "A", "the absent list is ignored, so a weak import gets an address after all",
     BOUNDARY,
     """                if request.weak && self.is_absent(request.name) {""",
     """                if false && self.is_absent(request.name) {""",
     ANDROID),

    # `time(tloc)` stores the value as well as returning it. A handler that returns the right
    # number and writes nothing is invisible to any test that only reads the return value.
    ("clocks-A8", "A", "time returns the right value and does not store it",
     ADAPTER_CLOCKS,
     """        if tloc != 0 {""",
     """        if false && tloc != 0 {""",
     ANDROID),

    # `CLOCKS_PER_SEC` is a million, fixed by POSIX. Reporting milliseconds is a constant
    # thousand-fold error in every ratio `clock()` is used to compute, and the value still rises.
    ("clocks-A9", "A", "clock reports milliseconds where CLOCKS_PER_SEC says microseconds",
     ADAPTER_CLOCKS,
     """        i64::try_from(cpu.as_micros()).map_err(|_| {""",
     """        i64::try_from(cpu.as_millis()).map_err(|_| {""",
     ANDROID),

    # The process CPU clock answered from the wall clock: monotonic, a plausible number of seconds,
    # and not what was asked for -- the exact failure `clocks-A1` records for `CLOCK_MONOTONIC`,
    # one clock along. Its detector is the one assertion a wall clock cannot satisfy: several
    # threads burning one interval of wall time advance a process CPU clock by more than it.
    ("plat-A9", "A", "process CPU time is served from the monotonic clock",
     PLAT_PROCESS_MOD,
     """pub fn cpu_time() -> ProcessResult<Duration> {
    backend::cpu_time()
}""",
     """pub fn cpu_time() -> ProcessResult<Duration> {
    let _ = backend::cpu_time();
    Ok(crate::clock::monotonic_now())
}""",
     PLATFORM),

    # `mallinfo` answering eighty zeroed bytes is the believable wrong answer precisely because it
    # is arithmetically TRUE of a libc heap nothing has allocated from -- and `libroblox.so`
    # imports no allocator at all, so there is no heap for it to describe.
    ("guestmem-A9", "A", "mallinfo writes eighty zeroed bytes instead of refusing",
     ADAPTER_GUESTMEM,
     """    let out = c.args().indirect_result();""",
     """    let out = c.args().indirect_result();
    if out != usize::MAX {
        c.mem().write_bytes(
            out,
            &[0u8; MALLINFO_BYTES],
            crate::mem::Blame::new(c.symbol(), c.address(), 0),
        )?;
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),

    # `longjmp` is declared `noreturn`. A handler that quietly returns resumes the guest in the
    # frame it was trying to escape, carrying whatever condition made it jump -- the same failure
    # `raise` declines, one frame further in.
    ("signals-A4", "A", "longjmp returns normally instead of refusing",
     ADAPTER_SIGNALS,
     """    let delivered = if val == 0 { 1 } else { val };""",
     """    let delivered = if val == 0 { 1 } else { val };
    if env != u64::MAX {
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),
    # ------------------------------------------------------------------ M3 task 4: the gate
    #
    # Running all 3,594 initializers is what found every defect below, and every row's detector is
    # an ordinary fast test rather than the gate itself: the gate loads 109 MB and executes 91.6 M
    # guest instructions, so a row scoped to it would multiply this harness's cost by that.

    # **The defect the gate cost the most to find.** Linux ignores `fd` entirely when
    # MAP_ANONYMOUS is set -- `mmap(2)` says so -- and this clause refused every anonymous
    # mapping made with the `0` the engine's own allocator passes. `libroblox.so` imports no
    # allocator, so guest `mmap` IS the heap seam: the gate went from 188 initializers to 3,096
    # on this one clause, and no existing test could see it because every one of them passes -1.
    ("guestmem-A10", "A", "an anonymous mmap with a non-negative fd is refused as file-backed",
     ADAPTER_GUESTMEM,
     """    if flags & MAP_ANONYMOUS == 0 {""",
     """    if fd != -1 || flags & MAP_ANONYMOUS == 0 {""",
     ANDROID),

    # `dlsym` answering with an `Unbound` slot's address. That address exists so a DIRECT call can
    # name the symbol; handing it back through `dlsym` converts a lookup the guest is prepared to
    # see fail into a pointer it will call thousands of initializers later -- which is exactly the
    # argument phase 2 refused all three `dl*` calls on, and the half of it that survives.
    ("boundary-A13", "A", "dlsym answers with an Unbound slot instead of missing",
     BOUNDARY,
     """        if matches!(slot.binding, Binding::Unbound) {
            return None;
        }""",
     """""",
     ANDROID),

    # The over-correction on the other side of the same function: a handle for one library
    # answering for every symbol this layer has. The guest's own `.gnu.version_r` says which
    # library each import comes from, and ignoring it makes `dlsym(libc_handle, "eglGetProcAddress")`
    # succeed where a device fails.
    ("boundary-B4", "B", "a library handle resolves symbols from every library",
     BOUNDARY,
     """            Some(name) if slot.library.as_deref() == Some(name) => Some(slot),
            Some(_) => None,""",
     """            Some(_) => Some(slot),""",
     ANDROID),

    # `dlopen` issuing a handle for a library this runtime does not have. NULL is the true answer
    # and the one every caller has a branch for; a handle makes the guest `dlsym` it and carry
    # what came back.
    ("dl-A5", "A", "dlopen issues a handle for a library this runtime does not supply",
     ADAPTER_DL,
     """                        ));
                        0
                    }""",
     """                        ));
                        HANDLE_TAG | (libraries.len() as u64 + 1)
                    }""",
     ANDROID),

    # `_SC_PAGESIZE` off by one. This is the shape D22 refused to risk for three phases: the real
    # page-size query then arrives as an unmodelled number and is refused LOUDLY, while some other
    # `_SC_` name silently receives a page size. The decode of the guest's own call sites is what
    # licensed the constant, so a row that moves it is a row about that evidence.
    ("procenv-A10", "A", "_SC_PAGESIZE is off by one",
     ADAPTER_PROCENV,
     """const SC_PAGESIZE: i32 = 0x0027;""",
     """const SC_PAGESIZE: i32 = 0x0026;""",
     ANDROID),

    # `PR_GET_THP_DISABLE` answering 0 -- "huge pages are available and not disabled" -- instead
    # of the EINVAL a kernel without CONFIG_TRANSPARENT_HUGEPAGE gives. Zero is the believable
    # wrong answer: it is a success, and the allocator then believes a feature exists.
    ("procenv-A11", "A", "the transparent-huge-page prctl options answer 0 instead of EINVAL",
     ADAPTER_PROCENV,
     """        view.set_errno(omni_bionic::errno::consts::EINVAL);
        drop(view);
        c.ret().i32(-1);
        return Ok(());""",
     """        drop(view);
        c.ret().i32(0);
        return Ok(());""",
     ANDROID),

    # `PR_SET_VMA` answering 0 without keeping the label. Keeping it is the ENTIRE observable
    # effect of that call on a device -- the text beside the range in /proc/self/maps -- so a
    # handler that returns 0 and stores nothing is the plausible stub, not an implementation.
    ("procenv-A12", "A", "PR_SET_VMA succeeds without recording the label",
     ADAPTER_PROCENV,
     """                Ok(text) => {
                    state.bionic.set_vma_name(addr, len, text);
                    0
                }""",
     """                Ok(_) => 0,""",
     ANDROID),

    # `gettid` answering the process id. It is the believable wrong answer precisely because it is
    # RIGHT for a single-threaded process -- on Linux the main thread's tid equals the pid -- and
    # it is what a naive implementation reaches for. Every thread would then share one identity.
    ("procenv-A13", "A", "gettid answers the process id instead of the thread identity",
     ADAPTER_PROCENV,
     """        let Ok(narrowed) = i32::try_from(thread.0) else {""",
     """        let thread = omni_bionic::threads::GuestThreadId(u64::from(
            omni_platform::process::pid(),
        ));
        let Ok(narrowed) = i32::try_from(thread.0) else {""",
     ANDROID),

    # `rt_sigprocmask` validating `how` before the `set` pointer. The engine passes an invalid
    # `how` ON PURPOSE and reads the errno to decide whether an address is readable; checking
    # `how` first answers EINVAL for every address, so the probe reports unmapped memory as
    # readable and the guest dereferences it.
    ("procenv-A14", "A", "rt_sigprocmask checks `how` before the pointer, breaking the probe",
     ADAPTER_PROCENV,
     """            if (set != 0 && !readable(&view, set, false))
                || (oldset != 0 && !readable(&view, oldset, true))
            {""",
     """            if false {""",
     ANDROID),

    # The over-correction: answering EFAULT for a readable `set` too. The probe then reports every
    # address as unreadable, and the guest routes around memory it could have used -- a wrong
    # answer with no failure anywhere.
    ("procenv-B5", "B", "rt_sigprocmask answers EFAULT for a readable set as well",
     ADAPTER_PROCENV,
     """            if (set != 0 && !readable(&view, set, false))
                || (oldset != 0 && !readable(&view, oldset, true))
            {""",
     """            if set != 0 || oldset != 0 {""",
     ANDROID),

    # `/dev/urandom` reading end-of-file. Zero is `/dev/null`'s answer, one entry along, and it is
    # what `std::random_device` gets when the file is not there -- the guest's C++ runtime then
    # throws `system_error` and terminates, which is how the gate found the device was needed.
    # A **short read** from `/dev/urandom`. A modern one always fills the buffer, and
    # `std::random_device` reads four bytes at a time with no loop -- so a partial fill leaves the
    # rest of the caller's buffer holding whatever was there, which is the believable wrong
    # answer: the bytes did change, and some of them are not entropy.
    ("plat-A11", "A", "/dev/urandom fills only half the buffer",
     PLAT_FS,
     """                    })?;
                    Ok(buf.len())
                }
                Device::Null => Ok(0),""",
     """                    })?;
                    Ok(buf.len() / 2)
                }
                Device::Null => Ok(0),""",
     PLATFORM),

    # The over-correction: every path under `/dev` becomes a device. A guest opening
    # `/dev/watchdog` would get a readable, writable character device instead of the ENOENT that
    # says this runtime does not have one, and the confinement rule would stop meaning anything
    # for that prefix.
    ("plat-B6", "B", "any path under /dev is treated as a device",
     PLAT_FS,
     """    DEVICES.iter().find(|(name, _)| *name == spelled).map(|(_, device)| *device)""",
     """    if spelled.starts_with("/dev/") {
        return Some(Device::Random);
    }
    DEVICES.iter().find(|(name, _)| *name == spelled).map(|(_, device)| *device)""",
     PLATFORM),

    # `mbtowc` reporting an illegal sequence as a successful zero-length decode. `0` is the value
    # for a NUL character, so the caller reads "end of string" and stops -- silently truncating
    # every string with a byte it could not decode, where a device answers -1 and EILSEQ.
    ("bionic-A12", "A", "mbtowc reports an illegal sequence as a NUL character",
     BIONIC_WIDE,
     """        Decode::Invalid | Decode::Incomplete => {
            ctx.set_errno(EILSEQ);
            Ok(-1)
        }""",
     """        Decode::Invalid | Decode::Incomplete => Ok(0),""",
     BIONIC),

    # POSIX `strerror_r` returning the buffer pointer, which is the GNU form's return value.
    # `libroblox.so` imports both spellings; a caller of the POSIX one tests the result against 0
    # and would read every success as a failure -- or, with a buffer at a low address, the other
    # way round.
    ("bionic-A13", "A", "POSIX strerror_r returns the buffer pointer like the GNU form",
     BIONIC_STRING,
     """    if message.len() >= len as usize {
        return Ok(crate::errno::consts::ERANGE);
    }
    Ok(0)""",
     """    Ok(b as i32)""",
     BIONIC),

    # =============================================================================================
    # M6 groundwork -- `omni-texture`, the ETC1 decoder. Added by the texture-transcoding task.
    #
    # A wrong decoder does not crash: it produces a plausible, silently wrong texture thousands of
    # frames before anyone looks at it. Every row below is a mutation that a casual test suite
    # would not notice, which is the whole reason the vectors in `tests/spec_vectors.rs` are
    # derived from the specification rather than from another decoder.
    # =============================================================================================

    # ---- the ETC2 escape: the one place an ETC1 decoder produces believable wrong pixels --------
    ("texture-A1", "A", "an ETC2 T/H/planar block is decoded as a differential block",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {
            return Err(modes[channel]);
        }""",
     """        if false && !(0..=31).contains(&sum) {
            return Err(modes[channel]);
        }""",
     TEXTURE),

    ("texture-A2", "A", "only the overflow end of the ETC2 escape is tested, not the underflow",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {""",
     """        if sum > 31 {""",
     TEXTURE),

    # The over-correction of the same check. Base 0 and base 31 are perfectly legal ETC1 endpoints
    # -- the real water-normal and skybox blocks use both -- and narrowing the range to refuse them
    # reads as "be strict about the boundary" while rejecting ordinary content.
    ("texture-B1", "B", "the ETC2 escape range is narrowed, refusing legitimate 0 and 31 endpoints",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {""",
     """        if !(1..=30).contains(&sum) {""",
     TEXTURE),

    # ---- the pixel index layout: the classic transposition ---------------------------------------
    ("texture-A3", "A", "pixel numbering is row-first, so every block is transposed",
     TEXTURE_ETC1,
     """            let i = x * BLOCK_EXTENT + y;""",
     """            let i = y * BLOCK_EXTENT + x;""",
     TEXTURE),

    ("texture-A4", "A", "the msb and lsb index bit planes are swapped",
     TEXTURE_ETC1,
     """            let lsb = (indices >> i) & 1;
            let msb = (indices >> (i + 16)) & 1;""",
     """            let lsb = (indices >> (i + 16)) & 1;
            let msb = (indices >> i) & 1;""",
     TEXTURE),

    # Specification table 8.16 maps 00 -> a, 01 -> b, 10 -> -a, 11 -> -b, which over the ascending
    # set {-b, -a, a, b} is elements 2, 3, 1, 0. The identity mapping is what a flattened table
    # copied in the wrong order gives, and it is a plausible-looking image.
    ("texture-A5", "A", "the pixel-index-to-modifier mapping is the identity",
     TEXTURE_ETC1,
     """const PIXEL_INDEX_TO_SET_ELEMENT: [usize; 4] = [2, 3, 1, 0];""",
     """const PIXEL_INDEX_TO_SET_ELEMENT: [usize; 4] = [0, 1, 2, 3];""",
     TEXTURE),

    # ---- base colour reconstruction ---------------------------------------------------------------
    ("texture-A6", "A", "5-to-8 bit extension shifts without replicating the high bits",
     TEXTURE_ETC1,
     """    (value << 3) | (value >> 2)""",
     """    value << 3""",
     TEXTURE),

    ("texture-A7", "A", "4-to-8 bit extension shifts without replicating the nibble",
     TEXTURE_ETC1,
     """    (value << 4) | value""",
     """    value << 4""",
     TEXTURE),

    ("texture-A8", "A", "the modifier wraps instead of saturating",
     TEXTURE_ETC1,
     """    let value = base as i32 + modifier;
    if value < 0 {
        0
    } else if value > 255 {
        255
    } else {
        value as u8
    }""",
     """    let value = base as i32 + modifier;
    value as u8""",
     TEXTURE),

    ("texture-A9", "A", "the diffbit is ignored, so every block decodes as differential",
     TEXTURE_ETC1,
     """    if (block[3] >> 1) & 1 == 0 {""",
     """    if false {""",
     TEXTURE),

    ("texture-A10", "A", "the flipbit is ignored, so the sub-block split is always left/right",
     TEXTURE_ETC1,
     """            let sub = usize::from(if flip { y >= 2 } else { x >= 2 });""",
     """            let sub = usize::from(x >= 2);""",
     TEXTURE),

    ("texture-A11", "A", "decoded alpha is transparent where an RGB format must read opaque",
     TEXTURE_ETC1,
     """            out[at + 3] = 0xFF;""",
     """            out[at + 3] = 0x00;""",
     TEXTURE),

    # ---- the format gate --------------------------------------------------------------------------
    # The believable one: ETC2's RGB8 form is a superset of ETC1 at the container level, so
    # accepting it here "obviously works" -- right up to the first block that uses a mode this
    # decoder does not have, which is content-dependent and therefore intermittent.
    ("texture-A12", "A", "GL_COMPRESSED_RGB8_ETC2 is accepted as though it were ETC1",
     TEXTURE_FORMAT,
     """        if gl_internal_format == 0x8D64 {""",
     """        if gl_internal_format == 0x8D64 || gl_internal_format == 0x9274 {""",
     TEXTURE),

    # ---- sizes, extents and the block grid --------------------------------------------------------
    ("texture-A13", "A", "the block grid truncates instead of rounding up",
     TEXTURE_LIB,
     """    let blocks_x = width / bw + u32::from(width % bw != 0);""",
     """    let blocks_x = width / bw;""",
     TEXTURE),

    ("texture-A14", "A", "an undersized destination is written into instead of refused",
     TEXTURE_LIB,
     """    if out.len() < needed_out {
        return Err(TextureError::OutputTooSmall {""",
     """    if false && out.len() < needed_out {
        return Err(TextureError::OutputTooSmall {""",
     TEXTURE),

    ("texture-A15", "A", "a zero extent is accepted and reports a zero-byte image",
     TEXTURE_LIB,
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 {""",
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if false && (width == 0 || height == 0) {""",
     TEXTURE),

    # ---- direction B: the three over-corrections ---------------------------------------------------
    # Each of these reads as "be stricter", and each destroys a property the design depends on.

    # A KTX mip level is padded to a four-byte boundary and a caller may hand over the rest of the
    # chain; GL's own `imageSize` is a lower bound, not an equality. Requiring an exact length
    # refuses the real APK's own files.
    ("texture-B2", "B", "a payload longer than the block grid is refused as truncated",
     TEXTURE_LIB,
     """    if data.len() < needed_in {
        return Err(TextureError::TruncatedBlockData {""",
     """    if data.len() != needed_in {
        return Err(TextureError::TruncatedBlockData {""",
     TEXTURE),

    ("texture-B3", "B", "a destination larger than the image is refused as too small",
     TEXTURE_LIB,
     """    let needed_out = decoded_len(width, height)?;
    if out.len() < needed_out {""",
     """    let needed_out = decoded_len(width, height)?;
    if out.len() != needed_out {""",
     TEXTURE),

    # The device limit that does not belong here. `maxImageDimension2D` is 32,768 on the
    # development host, but that is a property of a device and this crate has no device in it: the
    # only bound that belongs here is arithmetic. A limit invented at this layer silently caps the
    # renderer on hardware that could go higher.
    ("texture-B4", "B", "a maximum dimension is invented inside pure computation",
     TEXTURE_LIB,
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 {""",
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {""",
     TEXTURE),

    # GL permits a compressed image whose dimensions are not multiples of the block size; the
    # texels past the edge are discarded. Refusing them outright is what the engine does for DXT
    # (`ERROR: DXT texture dimension {}x{} not divisible by 4.` is in `libroblox.so`), which is
    # exactly what makes it a believable over-correction here.
    ("texture-B5", "B", "a non-multiple-of-four extent is refused instead of clipped",
     TEXTURE_LIB,
     """    let (bw, bh) = format.block_extent();""",
     """    let (bw, bh) = format.block_extent();
    if width % bw != 0 || height % bh != 0 {
        return Err(TextureError::ZeroExtent { width, height });
    }""",
     TEXTURE),


    # ======================================================================= M4: JNI without a JVM
    #
    # Every row below is a property jni-surface.md or this milestone's own measurements
    # established. The A rows revert a fix and something must fail; the B rows over-correct --
    # they read as more careful and destroy a property the design depends on.

    # **The defect M4's gate found.** A `jclass` is an instance of `java.lang.Class`, and
    # `JvmClassLoaderHelper` takes the class of a class and asks *that* for `getClassLoader`.
    # Answering the class itself makes the lookup ask `NativeGLJavaInterface.getClassLoader`,
    # which does not exist, and the null `jmethodID` goes straight into `CallObjectMethodV`.
    ("jni-A1", "A", "GetObjectClass on a jclass answers the class itself",
     JNI_ENV,
     """        Object::Class(_) => registry.find("java/lang/Class"),""",
     """        Object::Class(class) => Some(*class),""",
     ANDROID_LIB),

    # The handle check word ignored. A `jobject` the guest deleted and used again resolves to
    # whatever took its slot -- and `DeleteLocalRef` has 45 call sites, so that is a shape the
    # engine produces in the ordinary course of running.
    ("jni-A2", "A", "a stale or forged jobject is not caught by its check word",
     JNI_REFS,
     """        if (handle >> 28) & CHECK_MASK != self.check_word(slot.generation) {""",
     """        if false {""",
     ANDROID_LIB),

    # `DeleteLocalRef` on a global reference performed rather than reported. The two have
    # different lifetimes, and deleting the wrong one leaves a later call holding a handle it was
    # entitled to keep.
    ("jni-A3", "A", "a reference is deleted through the wrong kind of DeleteRef",
     JNI_REFS,
     """        if actual != expected {""",
     """        if false {""",
     ANDROID_LIB),

    # `Answer::Unanswered` evaluating to a value. This is Global Constraint 1's failure shape
    # exactly: a Java getter nobody decided the answer for returning a believable zero.
    ("jni-A4", "A", "a member nobody decided the answer for returns zero",
     JNI_CLASSES,
     """            Answer::Sink => Value::Void,""",
     """            Answer::Unanswered => Value::Int(0),
            Answer::Sink => Value::Void,""",
     ANDROID_LIB),

    # Modified UTF-8's first divergence from UTF-8: U+0000 is `C0 80`, never a zero byte. Writing
    # it as one byte terminates the string the guest is about to read at the first NUL character
    # in it.
    ("jni-A5", "A", "modified UTF-8 writes U+0000 as a single zero byte",
     JNI_VALUES,
     """                0x0000 | 0x0080..=0x07ff => {""",
     """                0x0000 => out.push(0),
                0x0080..=0x07ff => {""",
     ANDROID_LIB),

    # One entry of `JNINativeInterface` transposed. Every slot at or after it moves by one, so the
    # guest's `ldr Xt,[Xb,#imm]` reaches a different function than the one the offset names.
    ("jni-A6", "A", "two JNINativeInterface entries are transposed",
     JNI_SLOTS,
     """    "GetStringUTFLength",
    "GetStringUTFChars",""",
     """    "GetStringUTFChars",
    "GetStringUTFLength",""",
     ANDROID_LIB),

    # A `Release…` given a pointer that is not a live pin silently doing nothing. The buffer stays
    # pinned and the guest reads through a pointer it believes it has given back.
    ("jni-A7", "A", "releasing a pointer that was never pinned is a silent no-op",
     JNI_POOL,
     """        self.live.get(&at).copied().ok_or_else(|| AbiError::JniRefused {""",
     """        self.live.get(&at).copied().or(self.live.values().next().copied()).ok_or_else(|| AbiError::JniRefused {""",
     ANDROID_LIB),

    # The array-region bound removed. `GetByteArrayRegion` and `SetLongArrayRegion` take a
    # guest-chosen start and length, and without this the host reads or writes outside the
    # object's own storage.
    #
    # **This row was `wrapping_add` instead of `checked_add` and nothing caught it**, correctly:
    # both values have come through `usize::try_from` of an `i32`, so on a 64-bit host the sum
    # cannot overflow and the two are the same function. The comment on `region` says so now.
    ("jni-A8", "A", "an array region is not bounds-checked against its array",
     JNI_ENV,
     """    if end > len {""",
     """    if false {""",
     ANDROID_LIB),

    # The generated dex surface put **ahead** of the hand-written members instead of after them.
    # `Registry::method` takes the first match, so every decided answer is shadowed by the
    # generated `Unanswered` copy of the same member and the engine's first call to one refuses.
    #
    # The first attempt at this row made `extend_with` add a member it already had, and nothing
    # caught it -- correctly: a duplicate appended *after* the hand-written one is never reached.
    # Which is the property worth pinning, and it is the order and not the duplication.
    # **Precedence between the two class tables**: a hand-written decided answer must win over
    # the generated `Unanswered` copy of the same member. This replaces the guard *and* the
    # append with an unconditional front-insert, so the generated copy is what `Registry::method`
    # finds.
    #
    # It takes both halves because either one alone is behaviour-preserving, which two earlier
    # attempts at this row found the expensive way: a duplicate appended after the hand-written
    # member is never reached, and a front-insert alone never runs for a member that is already
    # there. The property is held by two independent things, so a row that breaks one of them is
    # not a detector -- and a row that looked like one would have been evidence for nothing.
    ("jni-A9", "A", "the generated surface wins over a decided answer",
     JNI_CLASSES,
     """            if self.declared_method(id, member.name, member.descriptor, member.is_static).is_none() {
                let class = &mut self.classes[usize::from(id.0)];
                if class.methods.len() < usize::from(u16::MAX) {
                    class.methods.push(Member {""",
     """            {
                let class = &mut self.classes[usize::from(id.0)];
                if class.methods.len() < usize::from(u16::MAX) {
                    class.methods.insert(0, Member {""",
     ANDROID_LIB),

    # `__strncpy_chk2`'s source check back to `n > src_size`, which aborts
    # `strncpy(dst, src, sizeof dst)` with a shorter source -- the commonest FORTIFY shape there
    # is, and what stopped jni-surface.md section 8 step 6 on the real engine.
    ("jni-A10", "A", "__strncpy_chk2 fails whenever n exceeds the source object",
     BIONIC_STRING,
     """    let readable = n.min(src_size);""",
     """    if n > src_size {
        return Err(crate::error::BionicError::CheckFailed("__strncpy_chk2"));
    }
    let readable = n.min(src_size);""",
     BIONIC),

    # `MADV_DONTNEED` degraded to `MADV_FREE`: marked idle and never reclaimed, so the old
    # contents survive a guarantee that says a later read is zero.
    ("jni-A11", "A", "MADV_DONTNEED marks the range idle and never reclaims it",
     ADAPTER_GUESTMEM,
     """        if let Err(error) = space.reclaim_idle() {""",
     """        if let Ok(()) = Ok::<(), omni_mem::MemError>(()) {
            c.invalidate_code(at, len)?;
            c.ret(|mut r| r.i32(0));
            return Ok(());
        }
        if let Err(error) = space.reclaim_idle() {""",
     ANDROID),

    # ---- B: the over-corrections -----------------------------------------------------------

    # Every JNI slot on the exit path. It reads as safer -- a handler that *may* call guest code
    # cannot then be on a path that structurally cannot -- and it destroys D17's measured split,
    # putting all 943 call sites on the 80-105 ns path instead of the 33 ns one.
    ("jni-B1", "B", "every JNIEnv slot is serviced on the exit path",
     JNI_ENV,
     """    name.starts_with("Call")
        || matches!(""",
     """    let _ = name;
    true
        || matches!(""",
     ANDROID_LIB),

    # The pinned pool committed eagerly. Four megabytes of commit charge per instance for a pool
    # that is empty until the engine asks for a string -- and this runtime hosts three concurrent
    # instances (Global Constraint 6, D10).
    ("jni-B2", "B", "the pinned pool is committed when it is reserved",
     JNI_POOL,
     """            CommitPolicy::Lazy,""",
     """            CommitPolicy::Eager,""",
     ANDROID_LIB),

    # `MADV_DONTNEED` unmapping the range. It is the *immediate* semantics taken one step too far:
    # the contents really do read as zero afterwards, and the mapping the guest still owns is
    # gone, so its next write faults.
    ("jni-B3", "B", "MADV_DONTNEED releases the mapping and not only the contents",
     ADAPTER_GUESTMEM,
     """        if let Err(error) = space.advise_idle(at, len) {""",
     """        if let Err(error) = space.unmap(at, len) {""",
     ANDROID),

    # The miss log bounded to one entry. It reads as tighter and it turns the measurement M5 is
    # built on into a sample of size one: a run that asked for forty members nobody declared
    # reports the first.
    ("jni-B4", "B", "the miss log keeps one entry instead of a thousand",
     JNI_CLASSES,
     """        if self.misses.len() < MAX_MISSES && !self.misses.contains(&miss) {""",
     """        if self.misses.len() < 1 && !self.misses.contains(&miss) {""",
     ANDROID_LIB),

    # Array types refused by the descriptor grammar. It reads as stricter -- an array is not a
    # class type -- and `[B`, `[I` and `[Ljava/lang/Object;` are all over the measured surface,
    # `showKeyboard` and `nativePassInputBatch` among them.
    ("jni-B5", "B", "the descriptor grammar refuses array types",
     JNI_VALUES,
     """            Some(b'[') => {""",
     """            Some(b'[') if false => {""",
     ANDROID_LIB),

    # The pinned-pool cap dropped to nothing. It reads as the safest possible bound and it makes
    # every `GetStringUTFChars` refuse, which is 45 of them on the startup path alone.
    ("jni-B6", "B", "the pinned pool refuses every pin",
     JNI_POOL,
     """        if self.pinned_bytes + need > MAX_PINNED_BYTES {""",
     """        if true {""",
     ANDROID_LIB),

    # =============================================== M5: the pipe, and the descriptor space opening
    #
    # `jni-surface.md` §5.2 needs two pipes before `initializeNativeCode` can return, and binding
    # `pipe` is what ended the closed-descriptor-space argument `bionic/net.rs` used to make. The
    # rows below are over the seam (`omni-platform`), the readiness rules `poll` and `select` now
    # answer from, and the blocking wait the adapter owns because the seam deliberately does not.

    # **The clause end-of-file depends on.** A reader whose writers have all closed must report
    # itself readable, because end of file IS a read that returns immediately. Without it a poll
    # loop parks on a pipe that can never produce another byte -- and every count-based assertion
    # about it still passes, which is `VERIFICATION.md` entry 11's shape exactly.
    ("pipe-A1", "A", "a reader with no writers left is not readable",
     PLAT_FS_PIPE,
     """                    readable: !empty || state.writers == 0,""",
     """                    readable: !empty,""",
     PLATFORM),

    # End of file itself: a read from an emptied, writerless pipe answers `WouldBlock` instead of
    # zero. A guest draining a pipe until `read` returns 0 never stops.
    ("pipe-A2", "A", "an emptied pipe with no writers reports EAGAIN instead of end of file",
     PLAT_FS_PIPE,
     """            if state.writers == 0 {
                // End of file, and it stays end of file: every later read answers zero too.
                return Ok(0);
            }""",
     """            if false {
                return Ok(0);
            }""",
     PLATFORM),

    # A write past the free space refusing the whole request rather than taking what fits. It is
    # the believable wrong answer for this shape: POSIX guarantees atomicity only to `PIPE_BUF`,
    # and a guest writing a large buffer would spin against a pipe that was draining.
    ("pipe-A3", "A", "a write larger than the free space takes nothing",
     PLAT_FS_PIPE,
     """        let taken = buf.len().min(room);
        state.queue.extend(&buf[..taken]);""",
     """        if buf.len() > room {
            return Err(FsError::kinded("write", "a pipe", FsErrorKind::WouldBlock, "no room"));
        }
        let taken = buf.len();
        state.queue.extend(&buf[..taken]);""",
     PLATFORM),

    # Closing an end not waking the gate. The last writer going away is what makes a blocked
    # reader see end of file; a close that does not raise the generation leaves that reader
    # parked until its deadline, which is a stall rather than a wrong answer -- the hardest kind
    # to see.
    ("pipe-A4", "A", "closing an end of a pipe does not wake what is waiting on it",
     PLAT_FS_PIPE,
     """        self.pipe.gate.bump();
    }
}

/// Create a pipe""",
     """        let _ = &self.pipe;
    }
}

/// Create a pipe""",
     PLATFORM),

    # `POLLHUP` masked by what was asked for. POSIX reports it whether or not it was requested,
    # and the canonical drain loop asks only for `POLLIN`: without this the loop never learns the
    # writer is gone.
    ("pipe-A5", "A", "POLLHUP is only reported when it was asked for",
     ADAPTER_NET,
     """    if readiness.hangup {
        revents |= POLLHUP;
    }""",
     """    if readiness.hangup {
        revents |= events & POLLHUP;
    }""",
     ANDROID),

    # The blocking wait removed: a blocking descriptor is told `EAGAIN`, which only a
    # non-blocking one can be told. The plausible wrong answer this whole type exists to prevent.
    ("pipe-A6", "A", "a blocking read reports EAGAIN instead of waiting",
     ADAPTER_FILES,
     """        errno == consts::EAGAIN && fs.is_nonblocking(fd).is_ok_and(|nonblocking| !nonblocking)""",
     """        let _ = (fs, fd, errno);
        false""",
     ANDROID),

    # The all-or-nothing descriptor check for a pipe's two ends. With `+ 1` a pipe fits where only
    # one slot is free, and the instance ends up holding one descriptor past its own ceiling.
    ("pipe-A7", "A", "a pipe needs only one free descriptor slot",
     PLAT_FS,
     """        if table.open.len() + 2 > MAX_OPEN_FILES {""",
     """        if table.open.len() + 1 > MAX_OPEN_FILES {""",
     PLATFORM),

    # `F_SETFL` accepting any bit, which is what Linux does and what this layer must not: a guest
    # that set `O_ASYNC` and was told it worked waits for a signal this runtime never delivers.
    ("pipe-A8", "A", "fcntl(F_SETFL) silently ignores every bit but O_NONBLOCK",
     ADAPTER_FILES,
     """                if unhandled != 0 {""",
     """                if false {""",
     ANDROID),

    # ---- the over-corrections ----

    # A pipe given the always-ready answer the other four kinds get. It reads as restoring the
    # simple rule `poll` used to have, and it makes every `poll` on an empty pipe report data
    # that is not there.
    ("pipe-B1", "B", "a pipe answers always-ready like every other descriptor kind",
     PLAT_FS,
     """            Entry::Pipe(handle) => handle.readiness(),""",
     """            Entry::Pipe(_) => Readiness::ALWAYS,""",
     PLATFORM),

    # The blocking bound removed. It reads as more POSIX-faithful -- a blocking read really does
    # wait indefinitely on a device -- and it is a permanent hang of a host thread, which D16's
    # step budgets cannot end because a sleeping thread executes no guest instructions.
    #
    # **Its detector is a unit test on the bound**, not an end-to-end one: an unbounded blocking
    # read on a pipe nobody writes to does not fail, it never returns. Same precedent, and same
    # reason, as the `clocks::capped` row.
    ("pipe-B2", "B", "a blocking transfer waits for ever, as a device does",
     ADAPTER_FILES,
     """    Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS)""",
     """    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)""",
     ANDROID_LIB),

    # `F_GETFL` reporting an access mode as well. It reads as more complete -- a real `F_GETFL`
    # does return one -- and this seam does not record which mode a descriptor was opened for, so
    # the value would be a guess the guest branches on.
    ("pipe-B3", "B", "F_GETFL invents an access mode",
     ADAPTER_FILES,
     """                Settled::Done(false) => 0,""",
     """                Settled::Done(false) => O_ACCMODE,""",
     ANDROID),

    # Reading the write end answered as end of file instead of `EBADF`. It reads as the gentler
    # answer and it tells a guest that used the wrong end of its own pipe that the data is gone.
    ("pipe-B4", "B", "reading the write end of a pipe is end of file rather than EBADF",
     PLAT_FS_PIPE,
     """        if self.end != PipeEnd::Read {
            return Err(FsError::kinded(
                "read",""",
     """        if self.end != PipeEnd::Read {
            return Ok(0);
        }
        if false {
            return Err(FsError::kinded(
                "read",""",
     PLATFORM),

    # ================================================================== M5: the ALooper
    #
    # §8.1's **fourth** failure mode is that `ALooper_forThread()` returning NULL makes
    # `initializeNativeCode` return 0 and Java-side startup fail silently. Every row here is about
    # one of the ways this layer could produce that silently, or produce a looper that answers
    # something a device would not.

    # The null that IS the answer, turned into a non-null. §5.2 decodes the constructor logging
    # "Unable to retrieve native ALooper" and returning zero on exactly this, so a layer that
    # always produced a looper would make the host's own precondition untestable.
    ("looper-A1", "A", "ALooper_forThread answers something rather than NULL when there is none",
     NDK_LOOPER,
     """    c.ret().u64(found.unwrap_or(0) as u64);""",
     """    c.ret().u64(found.unwrap_or(1) as u64);""",
     ANDROID),

    # Callbacks run before an ident is reported. AOSP reports idents first, and the glue depends
    # on it: `android_app_entry` registers its command pipe with LOOPER_ID_MAIN and no callback,
    # and `GameLoop` switches on the return.
    ("looper-A2", "A", "a callback is run where an ident should have been reported",
     NDK_LOOPER,
     """            match ident {
                Some(found) => found,
                None if callbacks.is_empty() => Pass::Idle,
                None => Pass::Callbacks(callbacks),
            }""",
     """            match ident {
                _ if !callbacks.is_empty() => Pass::Callbacks(callbacks),
                Some(found) => found,
                None => Pass::Idle,
            }""",
     ANDROID),

    # A registration with a callback keeping the caller's ident. §5.2's constructor passes
    # `ident = 0` **and** a callback, so this makes `pollOnce` report ident 0 -- a legal-looking
    # answer the glue has no branch for.
    ("looper-A3", "A", "a callback registration keeps an ident that can never be reported",
     NDK_LOOPER,
     """        ident: if callback == 0 { ident } else { ALOOPER_POLL_CALLBACK },""",
     """        ident,""",
     ANDROID),

    # A callback returning zero no longer removes its registration, which is the NDK's documented
    # contract and the mechanism by which the glue detaches its pipe.
    ("looper-A4", "A", "a callback returning zero does not remove its registration",
     NDK_LOOPER,
     """                if returned == 0 {""",
     """                if false && returned == 0 {""",
     ANDROID),

    # The last release no longer destroys the looper, so the thread keeps one for ever and
    # `ALooper_forThread` can never answer NULL again -- which removes the very condition §8.1's
    # fourth failure mode is about.
    ("looper-A5", "A", "the last release leaves the looper alive",
     NDK_LOOPER,
     """    if references == 0 {
        // The last reference""",
     """    if false {
        // The last reference""",
     ANDROID),

    # A descriptor the instance does not hold accepted into a looper. Its readiness would then
    # have to be invented, which is the whole reason the check is there.
    ("looper-A6", "A", "a looper accepts a descriptor this runtime does not have",
     NDK_LOOPER,
     """        if !fs.is_open(fd) {""",
     """        if false {""",
     ANDROID),

    # `pollOnce` returns the right ident and writes no `outFd`. A caller that read the stale value
    # acts on whatever descriptor was there last.
    ("looper-A7", "A", "pollOnce reports an ident and writes no out-parameter",
     NDK_LOOPER,
     """            if out_fd != 0 {""",
     """            if false {""",
     ANDROID),

    # The wait removed: `pollOnce` answers POLL_TIMEOUT immediately for any timeout. A game loop
    # would spin at whatever rate the run budget allowed instead of waiting for its pipe.
    # STALE PATTERN REPAIRED (M6). The indefinite wait moved the deadline test inside
    # `Bound::Until`'s arm, so the four lines moved two levels in and this row had been matching
    # nothing -- a silent MISS with no relation to the test it names. Found by the whole-table
    # pattern pass, which is what `VERIFICATION.md` entry 8 says to run; the row's intent is
    # unchanged.
    ("looper-A8", "A", "pollOnce never waits, and times out at once",
     NDK_LOOPER,
     """                let now = Instant::now();
                if now >= deadline {
                    break Pass::Idle;
                }""",
     """                let now = Instant::now();
                if true {
                    break Pass::Idle;
                }""",
     ANDROID),

    # ---- the over-corrections ----

    # `ALooper_prepare` taking a reference for its caller as well as the thread's. It reads as the
    # careful thing -- the caller has a pointer, so surely it holds a reference -- and it leaves
    # the count one too high for ever, so the looper outlives the thread that owns it.
    ("looper-B1", "B", "prepare takes a reference for the caller as well as the thread",
     NDK_LOOPER,
     """        Looper { thread, opts, references: 1, fds: Vec::new() }""",
     """        Looper { thread, opts, references: 2, fds: Vec::new() }""",
     ANDROID),

    # The indefinite-wait refusal widened to every non-positive timeout. It reads as stricter, and
    # `ALooper_pollOnce(0, ..)` is the ordinary non-blocking poll a game loop makes every frame.
    #
    # **STALE SINCE M6, AND DELIBERATELY NOT REPAIRED HERE.** `let budget =` is `let bound =` now,
    # so this pattern matches nothing and the row is a silent MISS -- found by the whole-table
    # pattern pass (entry 8). The one-word repair is **not** safe to make without running it: M6
    # turned the `< 0` branch from a refusal into a *wait*, so `<= 0` no longer refuses a zero
    # timeout, it makes one wait indefinitely whenever a live write end exists. The stopping test
    # would catch it, but any other `pollOnce(0)` over a live pipe with nothing ready would hang
    # the target instead of failing it, and `run()` here has no timeout. Repair it with a `new`
    # that cannot park -- and prove that by reading every `pollOnce(0)` call site in
    # `tests/ndk.rs`, not by running the harness and finding out.
    ("looper-B2", "B", "a zero timeout is refused along with an indefinite one",
     NDK_LOOPER,
     """    let budget = if timeout_millis < 0 {""",
     """    let budget = if timeout_millis <= 0 {""",
     ANDROID),

    # A second `addFd` for one descriptor keeping both registrations. It reads as losing nothing,
    # and it reports one descriptor twice -- so a `pollOnce` that should answer one ident answers
    # a callback as well.
    ("looper-B3", "B", "a second addFd for one descriptor keeps both registrations",
     NDK_LOOPER,
     """    entry.fds.retain(|held| held.fd != fd);
    if entry.fds.len() >= MAX_LOOPER_FDS {""",
     """    if entry.fds.len() >= MAX_LOOPER_FDS {""",
     ANDROID),

    # ---- the park witness, for §8 row 14 ----

    # The guard stops removing its entry. A stale park makes a run that finished look like the
    # deadlock the witness exists to find, which is worse than having no witness at all.
    ("park-A1", "A", "a thread that finished waiting stays recorded as parked",
     ADAPTER_MOD,
     """        if let Some(index) = parked.iter().position(|held| held.started == self.token) {
            parked.remove(index);
        }""",
     """        if let Some(index) = parked.iter().position(|held| held.started == self.token) {
            let _ = index;
        }""",
     ANDROID),

    # The witness records the mutex where the condition variable belongs. **A substitution, not a
    # count**: the number of parked threads is right, every total is right, and the object named
    # is the wrong one -- which is this project's first verification lesson, applied to its own
    # newest instrument.
    # ============================================ M5: the Win32 device set, measured rather than listed
    #
    # Review finding M6 said `WINDOWS_DEVICES` omitted `COM0`/`LPT0` and the superscript forms.
    # **Half of that was wrong**, and the measurement is what settled it: `COM0` and `LPT0` are
    # ordinary files on this build (`RtlIsDosDeviceName_U` answers zero for both, and
    # `CreateFileW("COM0")` created a file the directory listing then showed), while `CONIN$`,
    # `CONOUT$` and the six U+00B9/B2/B3 forms are real devices and really were missing. So there
    # is an A row for each half that was missing and a **B row for believing the finding as
    # filed** -- adding `COM0`/`LPT0` is the over-correction, and it now fails.

    # **The anchor is the whole const, because the declared length has to move with the
    # elements.** A first version dropped entries and left `[&str; 30]`, which does not compile --
    # and the harness reported it as `did not compile, twice` rather than as `caught`, which is
    # the retry gate doing exactly what `VERIFICATION.md` entry 8 added it for. A row that does not
    # compile proves nothing, and it is the one outcome that looks like a result.
    ("confine-A1", "A", "the console-buffer device names are dropped from the list",
     PLAT_FS_PATH,
     r"""const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",
    // The console buffers, openable by name — the first half of M6.
    "CONIN$", "CONOUT$",
    // Serial and parallel ports. The range is 1..=9: `COM0` and `LPT0` are files, measured.
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    // The superscript forms, U+00B9/U+00B2/U+00B3 — the second half of M6. Only these three
    // digit look-alikes match; twenty-six others were tried and did not.
    "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];""",
     r"""const WINDOWS_DEVICES: [&str; 28] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",
    // Serial and parallel ports. The range is 1..=9: `COM0` and `LPT0` are files, measured.
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    // The superscript forms, U+00B9/U+00B2/U+00B3 — the second half of M6. Only these three
    // digit look-alikes match; twenty-six others were tried and did not.
    "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];""",
     PLATFORM),

    # As `confine-A1`: the whole const, so the length moves with the elements.
    ("confine-A2", "A", "the superscript COM/LPT device forms are dropped from the list",
     PLAT_FS_PATH,
     r"""const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",
    // The console buffers, openable by name — the first half of M6.
    "CONIN$", "CONOUT$",
    // Serial and parallel ports. The range is 1..=9: `COM0` and `LPT0` are files, measured.
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    // The superscript forms, U+00B9/U+00B2/U+00B3 — the second half of M6. Only these three
    // digit look-alikes match; twenty-six others were tried and did not.
    "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];""",
     r"""const WINDOWS_DEVICES: [&str; 24] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",
    // The console buffers, openable by name — the first half of M6.
    "CONIN$", "CONOUT$",
    // Serial and parallel ports. The range is 1..=9: `COM0` and `LPT0` are files, measured.
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];""",
     PLATFORM),

    # The extension no longer stripped, so `NUL.txt` reaches the host. MEASURED as *not* a device
    # on this build -- and refused anyway, because the confinement property must not depend on a
    # Windows build number and an over-refusal cannot create an escape.
    ("confine-A3", "A", "a device name with an extension is no longer recognised",
     PLAT_FS_PATH,
     """    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');""",
     """    let stem = name;""",
     PLATFORM),

    # The trailing space no longer trimmed, so `NUL .txt` passes. One strip away from a device.
    ("confine-A4", "A", "a device name with a trailing space is no longer recognised",
     PLAT_FS_PATH,
     """    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');""",
     """    let stem = name.split('.').next().unwrap_or(name);""",
     PLATFORM),

    # **Believing review finding M6 as it was filed.** It asked for `COM0`/`LPT0`; both measured
    # as ordinary files. Adding them costs a guest two filenames it is entitled to, which is the
    # over-refusal direction -- safe for confinement and wrong as a statement about the host.
    ("confine-B1", "B", "COM0 and LPT0 are refused, which the measurement says are files",
     PLAT_FS_PATH,
     """const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",""",
     """const WINDOWS_DEVICES: [&str; 32] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL", "COM0", "LPT0",""",
     PLATFORM),

    # Generalising the three superscripts into an "any digit look-alike" rule, by adding the
    # fullwidth form. MEASURED not to be a device; twenty-six were tried and only three matched.
    ("confine-B2", "B", "the superscript special case is generalised to another digit look-alike",
     PLAT_FS_PATH,
     """const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",""",
     r"""const WINDOWS_DEVICES: [&str; 31] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL", "COM\u{ff11}",""",
     PLATFORM),

    # `cpu_count` back to substituting a believable `1`, which is review finding M7's own shape:
    # the pattern `random_bytes` forbids eleven lines below it in the same file.
    ("confine-A5", "A", "cpu_count substitutes a believable 1 instead of reporting failure",
     PLAT_PROCESS_MOD,
     """    std::thread::available_parallelism().map_err(|error| ProcessError::Indeterminate {
        operation: "cpu_count",
        detail: error.to_string(),
    })""",
     """    Ok(std::num::NonZeroUsize::new(1).expect("one is not zero"))""",
     PLATFORM),

    # And the over-correction in the other direction: a fabricated `Unsupported` for a primitive
    # that is one portable `std` call on all five targets, which `lib.rs` forbids by name.
    ("confine-B3", "B", "cpu_count claims to be unsupported where std answers on every target",
     PLAT_PROCESS_MOD,
     """    std::thread::available_parallelism().map_err(|error| ProcessError::Indeterminate {
        operation: "cpu_count",
        detail: error.to_string(),
    })""",
     """    Err(ProcessError::Unsupported {
        operation: "cpu_count",
        intended: "sysconf(_SC_NPROCESSORS_ONLN)",
        platform: "this target",
    })""",
     PLATFORM),

    # =============================================================== M5: strftime, and its refusals
    #
    # `nativeInitFastLog` is one of the two scripted downcalls of §8 step 9 that did not return,
    # and M4 recorded why: there was no implementation to bind. These rows are over the one that
    # now exists. Every `A` row is a **plausible** wrong answer -- an off-by-one in a day number,
    # a twelve-hour clock that prints `00` at noon -- because wrong *text* is what this function
    # fails as, and nothing downstream can tell wrong text from right text.

    # The NUL stops needing room, so a result that exactly fills `max` reports as fitting. C says
    # the array is indeterminate in that case, so the caller would read an unterminated string.
    ("strftime-A1", "A", "the terminating NUL stops needing room in max",
     BIONIC_TIME,
     """    if out.len() < max {""",
     """    if out.len() <= max {""",
     BIONIC),

    # `%V` loses ISO 8601's Thursday rule and becomes a plain Monday-week count. Agrees with the
    # right answer for most of the year, which is what makes it worth a row.
    ("strftime-A2", "A", "%V loses ISO's Thursday rule and counts Mondays instead",
     BIONIC_TIME,
     """    let week = (yday - iso_wday + 10) / 7;""",
     """    let week = (yday - iso_wday + 7) / 7;""",
     BIONIC),

    # `%e` zero-pads like `%d`. The two conversions exist precisely to differ.
    ("strftime-A3", "A", "%e zero-pads, so it stops differing from %d",
     BIONIC_TIME,
     """        b'e' => push_padded(out, mday(tm, specifier)? as u64, 2, b' '),""",
     """        b'e' => push_padded(out, mday(tm, specifier)? as u64, 2, b'0'),""",
     BIONIC),

    # An unknown conversion copied through as literal text instead of refused. tzcode-derived
    # libraries do exactly this, and `%Q` in a log line is indistinguishable from a time.
    ("strftime-A4", "A", "an unknown conversion is copied through instead of refused",
     BIONIC_TIME,
     """        _ => return Err(StrftimeError::UnknownSpecifier { specifier }),""",
     """        _ => { out.push(b'%'); out.push(specifier); }""",
     BIONIC),

    # `%j` loses the 0-based-to-1-based `+1`. **Still three digits, still parses, off by one for
    # every day of every year** -- the shape a count-based test cannot see.
    ("strftime-A5", "A", "%j is off by one because tm_yday is zero-based",
     BIONIC_TIME,
     """        b'j' => push_padded(out, (yday(tm, specifier)? + 1) as u64, 3, b'0'),""",
     """        b'j' => push_padded(out, yday(tm, specifier)? as u64, 3, b'0'),""",
     BIONIC),

    # `%I` loses the twelve-for-zero case, so noon and midnight print `00`. Wrong for two hours a
    # day and right for the other twenty-two.
    ("strftime-A6", "A", "%I prints 00 at noon and midnight",
     BIONIC_TIME,
     """            let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };""",
     """            let twelve = hour % 12;""",
     BIONIC),

    # ---- the over-corrections ----

    # `tm_sec` narrowed to [0,59]. It reads as "seconds are 0 to 59" and destroys the leap second
    # POSIX's own `<time.h>` range admits.
    ("strftime-B1", "B", "tm_sec is narrowed to 0..=59 and the leap second is refused",
     BIONIC_TIME,
     """        b'S' => push_padded(out, field(tm.sec, "tm_sec", 0, 60, specifier)? as u64, 2, b'0'),""",
     """        b'S' => push_padded(out, field(tm.sec, "tm_sec", 0, 59, specifier)? as u64, 2, b'0'),""",
     BIONIC),

    # `%z` refuses an offset that is not a whole minute. It reads as "never silently lose
    # seconds" and destroys POSIX's documented truncation, which is what `+hhmm` can express.
    ("strftime-B2", "B", "%z refuses an offset that is not a whole number of minutes",
     BIONIC_TIME,
     """            if !(-86_400..=86_400).contains(&offset) {""",
     """            if !(-86_400..=86_400).contains(&offset) || offset % 60 != 0 {""",
     BIONIC),

    # The output cap cut to 4 KiB. It reads as "a timestamp is never longer than a log line" and
    # refuses formats the thunk boundary can actually deliver.
    ("strftime-B3", "B", "the strftime output cap is cut to a log line's worth",
     BIONIC_TIME,
     """pub const MAX_STRFTIME_OUTPUT: usize = 1024 * 1024;""",
     """pub const MAX_STRFTIME_OUTPUT: usize = 4096;""",
     BIONIC),

    # ============================================== M5: the fopen mode parse, review finding M5
    #
    # The finding was that a mode over sixteen bytes was **silently truncated**, so
    # `"rbbbbbbbbbbbbbbb+"` lost its `+` and yielded a read-only stream while the code's own doc
    # said `EINVAL`. It also named a sixteen-byte witness that parsed correctly even with the
    # bound -- the shape begins at seventeen -- which is `VERIFICATION.md` entry 10 again: the
    # conclusion was right and the evidence for it was not.

    # The bound put back, in the place it now would have to go.
    ("modes-A1", "A", "a long fopen mode is truncated again, losing its trailing +",
     BIONIC_STDIO,
     """    let mode = match mode.iter().position(|&byte| byte == 0) {
        Some(end) => &mode[..end],
        None => mode,
    };""",
     """    let mode = match mode.iter().position(|&byte| byte == 0) {
        Some(end) => &mode[..end],
        None => &mode[..mode.len().min(16)],
    };""",
     BIONIC),

    # `+` only adds write, so `w+` and `a+` are not readable. C17 7.21.5.3p3 says update modes
    # permit both.
    ("modes-A2", "A", "+ only adds write, so w+ and a+ are not readable",
     BIONIC_STDIO,
     """        read: reads || plus,
        write: writes || plus,""",
     """        read: reads,
        write: writes || plus,""",
     BIONIC),

    # An unknown modifier ignored instead of refused. An unknown byte *might* change the access
    # on a device, and ignoring it hands back a stream whose access differs from the one asked
    # for -- which is the finding's own shape, one character along.
    ("modes-A3", "A", "an unknown mode modifier is ignored instead of refused",
     BIONIC_STDIO,
     """            other => return Err(ModeRefusal::Modifier(other)),""",
     """            _ => {}""",
     BIONIC),

    # An interior NUL is not a terminator, so `"r\0b+"` grants write access the C string never
    # asked for.
    ("modes-A4", "A", "an interior NUL stops ending the mode string",
     BIONIC_STDIO,
     """    let mode = match mode.iter().position(|&byte| byte == 0) {
        Some(end) => &mode[..end],
        None => mode,
    };
    let Some((&access, modifiers)) = mode.split_first() else {""",
     """    let Some((&access, modifiers)) = mode.split_first() else {""",
     BIONIC),

    # ---- the over-corrections ----

    # `x` honoured with no creating mode. POSIX: "if O_EXCL is set and O_CREAT is not set, the
    # result is undefined" -- so there is no defined thing to pass on.
    ("modes-B1", "B", "x sets O_EXCL with no O_CREAT, which POSIX leaves undefined",
     BIONIC_STDIO,
     """        exclusive: exclusive && create,""",
     """        exclusive,""",
     BIONIC),

    # Making the code match the OLD doc instead of removing the bound: loud rather than silent,
    # and still wrong, because bionic's `__sflags` walks to the terminator and opens these.
    ("modes-B2", "B", "a mode past sixteen bytes is refused rather than parsed",
     BIONIC_STDIO,
     """    let Some((&access, modifiers)) = mode.split_first() else {
        return Err(ModeRefusal::Empty);
    };""",
     """    if mode.len() > 16 {
        return Err(ModeRefusal::Modifier(b'?'));
    }
    let Some((&access, modifiers)) = mode.split_first() else {
        return Err(ModeRefusal::Empty);
    };""",
     BIONIC),

    # ================================= M5: the log ring's byte bound, and liblog's truncation
    #
    # Review findings M3 and M4. The ring bounded record count, not bytes, so 256 records of a
    # guest-chosen megabyte each was 0.8 GiB with `log_dropped()` still reporting 0; and
    # `__android_log_print` refused where a real `liblog` truncates.

    # The byte bound set so high it can never bind -- which is a check no input can fail, and is
    # `VERIFICATION.md` entry 12's shape applied to a bound rather than a branch.
    ("log-A1", "A", "the ring's byte bound is set where it can never bind",
     ADAPTER_LOGGING,
     """pub const LOG_CAPTURE_MAX_BYTES: usize = 256 * 1024;""",
     """pub const LOG_CAPTURE_MAX_BYTES: usize = 64 * 1024 * 1024;""",
     ANDROID_LIB),

    # The ring stops checking bytes at all, which is what M3 found.
    ("log-A2", "A", "the ring bounds records only, as it did before M3",
     ADAPTER_LOGGING,
     """            let over_bytes = state.bytes.saturating_add(cost) > LOG_CAPTURE_MAX_BYTES;""",
     """            let over_bytes = false;""",
     ANDROID_LIB),

    # **A record is truncated and does not say so.** The whole point of reproducing the platform's
    # truncation rather than refusing was that it be visible; a log line silently missing its tail
    # is a wrong answer a reader cannot detect.
    ("log-A3", "A", "a truncated record does not record that it was truncated",
     ADAPTER_LOGGING,
     """    let truncated = if kept.tag < tag.len() || kept.message < message.len() {
        Some(Truncation { tag_bytes: tag.len(), message_bytes: message.len() })
    } else {
        None
    };""",
     """    let truncated = None;""",
     ANDROID_LIB),

    # The same thing on the other reporting channel: the stderr line drops its marker, so a run
    # watched only through stderr goes silently short.
    ("log-A4", "A", "the stderr line drops the truncation marker",
     PLAT_LOG,
     """    if let Some(cut) = record.truncated {""",
     """    if let Some(cut) = record.truncated.filter(|_| false) {""",
     PLATFORM),

    # An eviction also bumps the truncation counter, so "dropped" and "truncated" stop being
    # distinguishable -- which is the measurement both bounds exist to provide.
    ("log-A5", "A", "an evicted record is also counted as truncated",
     ADAPTER_LOGGING,
     """            self.dropped.fetch_add(1, Ordering::Relaxed);""",
     """            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.truncated.fetch_add(1, Ordering::Relaxed);""",
     ANDROID_LIB),

    # The per-record ceiling forgets that one invalid guest byte becomes a three-byte U+FFFD, so
    # the bound is 8,130 bytes per record too small and the ring can exceed it.
    ("log-A6", "A", "the per-record ceiling forgets that a bad byte widens threefold",
     ADAPTER_LOGGING,
     """    core::mem::size_of::<LogRecord>() + 3 * MAX_TAG_AND_MESSAGE_BYTES;""",
     """    core::mem::size_of::<LogRecord>() + MAX_TAG_AND_MESSAGE_BYTES;""",
     ANDROID_LIB),

    # ---- the over-corrections ----

    # The message capped at the payload cap instead of liblog's own 1024-byte buffer, so a line a
    # device would have cut at 1023 survives whole. Under-truncating reads as generous.
    ("log-B1", "B", "the message is capped at the payload rather than liblog's buffer",
     PLAT_LOG,
     """    let message = message_bytes.min(MAX_MESSAGE_BYTES);""",
     """    let message = message_bytes.min(MAX_TAG_AND_MESSAGE_BYTES);""",
     PLATFORM),

    # Cutting far below the platform's cap, so a line a device carries whole comes back short.
    ("log-B2", "B", "the message buffer is cut far below liblog's own",
     PLAT_LOG,
     """pub const LOG_BUF_SIZE: usize = 1024;""",
     """pub const LOG_BUF_SIZE: usize = 256;""",
     PLATFORM),

    # The payload cap fills the **message** first and cuts the tag -- "keep the useful part",
    # which is the opposite of the order `logd_writer.cpp`'s three iovecs impose.
    ("log-B3", "B", "the payload cap fills the message first and cuts the tag",
     PLAT_LOG,
     """    let tag = tag_bytes.min(MAX_TAG_AND_MESSAGE_BYTES);""",
     """    let tag = tag_bytes.min(MAX_TAG_AND_MESSAGE_BYTES.saturating_sub(message));""",
     PLATFORM),

    ("park-A2", "A", "the park witness names the mutex as the condition variable",
     ADAPTER_HANDLERS,
     """    let _parked = state.bionic.park("pthread_cond_wait", state.thread, cond, mutex, stack);""",
     """    let _parked = state.bionic.park("pthread_cond_wait", state.thread, mutex, mutex, stack);""",
     ANDROID),

    # ---- ANativeWindow (M6). The five symbols `libroblox.so` imports. -------------------------

    # The last release no longer frees the slot, so a destroyed window stays live and a release
    # past the last reference silently succeeds -- a handle guest code believes it gave up.
    ("window-A1", "A", "the last release no longer frees the window slot",
     NDK_WINDOW,
     """        state.windows.remove(at);""",
     """        let _ = at;""",
     ANDROID),

    # `_getHeight` answers the width. Both are `int32_t` through the same path, so nothing but a
    # test that asserts the two DIFFER can see it -- which is why the detector sets them apart.
    ("window-A2", "A", "ANativeWindow_getHeight answers the width",
     NDK_WINDOW,
     """    c.ret().i32(geometry.height);""",
     """    c.ret().i32(geometry.width);""",
     ANDROID),

    # The undecided geometry becomes an invented 1920x1080 -- the exact wrong answer the module
    # documents its refusal against, and the one that is indistinguishable in every log from a
    # resolution the host meant.
    ("window-A3", "A", "an undecided window geometry is invented as 1920x1080",
     NDK_MOD,
     """    pub fn window_geometry(&self) -> Option<WindowGeometry> {
        *self.window_geometry.lock()
    }""",
     """    pub fn window_geometry(&self) -> Option<WindowGeometry> {
        Some(self.window_geometry.lock().unwrap_or(WindowGeometry { width: 1920, height: 1080 }))
    }""",
     ANDROID),

    # The `Surface` class check is dropped, so any live jobject becomes a window -- an
    # AssetManager passed by mistake answers as a surface thousands of instructions later.
    ("window-A4", "A", "ANativeWindow_fromSurface stops checking the jobject's class",
     NDK_WINDOW,
     """    if class != SURFACE_CLASS {""",
     """    if false {""",
     ANDROID),

    # `_getWidth` stops checking the handle, so a forged pointer reads the host's geometry and a
    # wrong handle is never reported at all.
    ("window-A5", "A", "ANativeWindow_getWidth stops checking the handle",
     NDK_WINDOW,
     """    // The handle is checked **before** the geometry, so a forged pointer is refused as a forged
    // pointer whether or not the host has decided: the two failures have different fixes.
    window_at(&ndk, c, window)?;""",
     """    let _ = window;""",
     ANDROID),

    # Over-corrects: `_fromSurface` refuses when the geometry is undecided. It reads as more
    # careful and is strictly worse -- it refuses a call that needs nothing this layer lacks, and
    # moves the diagnosis away from the call that actually wanted the number.
    ("window-B1", "B", "fromSurface refuses when the geometry is undecided",
     NDK_WINDOW,
     """    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    let existing =""",
     """    decided(&ndk, c)?;
    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    let existing =""",
     ANDROID),

    # Over-corrects: the window ceiling refuses instead of answering null, inventing a failure
    # mode §8 row 17's caller has no arm for. A device that cannot produce a window answers null.
    ("window-B2", "B", "the window ceiling refuses instead of answering null",
     NDK_WINDOW,
     """        drop(state);
        // **Null, not a refusal.** A device that cannot produce a window answers null here and
        // §8 row 17's caller branches on it; a cap this layer chose is not a reason to invent a
        // failure mode the caller has no arm for.
        c.ret().u64(0);
        return Ok(());""",
     """        drop(state);
        return Err(refuse(c, format!("this instance already holds {} ANativeWindows", super::MAX_NATIVE_WINDOWS)));""",
     ANDROID),

    # Over-corrects: each `_fromSurface` hands out a FRESH window rather than a second reference.
    # It reads as the more careful, less-sharing choice, and it makes §8 row 17's "release any old
    # window" destroy an object another holder still has.
    ("window-B3", "B", "fromSurface hands out a fresh window instead of a second reference",
     NDK_WINDOW,
     """    let existing =
        state.windows.iter().find(|(_, live)| live.from_surface == surface).map(|(at, _)| at);""",
     """    let existing: Option<GuestAddr> = None;""",
     ANDROID),

    # ---- Review finding M1: admit the guest buffer BEFORE the descriptor is touched. ----------
    #
    # What these rows defend is an ORDER, not a check. The unfixed code validated per chunk, which
    # also never wrote out of bounds -- it just reported the failure as a SHORT COUNT, and a short
    # read is how a drained pipe and an ended file announce themselves. So every detector below
    # asserts on the DESCRIPTOR, not on guest memory: guest memory looks identical either way.

    ("order-A1", "A", "read/pread consume from the descriptor before the destination is admitted",
     ADAPTER_FILES,
     """    let base = transfer_buffer(view, buffer, count, true, 1)?;""",
     """    let base = guest_address(view, buffer)?;""",
     ANDROID),

    ("order-A2", "A", "write/__write_chk reach the descriptor before the source is admitted",
     ADAPTER_FILES,
     """    let base = transfer_buffer(view, buf, count, false, 1)?;""",
     """    let base = guest_address(view, buf)?;""",
     ANDROID),

    # The believable wrong fix: only the first byte is admitted rather than the whole length. It
    # looks like a validated transfer and catches a wholly unmapped buffer, which is the case a
    # careless test would use -- and it misses every buffer that starts mapped and ends nowhere.
    ("order-A3", "A", "only the first byte of the transfer buffer is admitted",
     ADAPTER_FILES,
     """    view.mem().checked_ptr(at, len, write, Blame::new(view.symbol(), view.address(), argument))?;""",
     """    view.mem().checked_ptr(at, len.min(1), write, Blame::new(view.symbol(), view.address(), argument))?;""",
     ANDROID),

    # `readdir` advances the stream before admitting the slot. There is no `seekdir` here, so the
    # entry it consumed and could not place is one the guest can never obtain again.
    ("order-A4", "A", "readdir advances the stream before admitting its slot",
     ADAPTER_FILES,
     """        let slot = guest_address(&view, dirp)?;
        view.mem().checked_ptr(
            slot,
            DIRENT_BYTES,
            true,
            Blame::new(view.symbol(), view.address(), 0),
        )?;
""",
     """        let _ = guest_address(&view, dirp)?;
""",
     ANDROID),

    # Over-corrects: validate unconditionally, so a legal `read(fd, NULL, 0)` refuses. C says a
    # zero-length read does not even check the descriptor for readability.
    ("order-B1", "B", "a zero-length read at a null pointer is refused",
     ADAPTER_FILES,
     """        if count == 0 {
            // C: zero bytes, and the descriptor is not even checked for readability by POSIX.
            // The seam is not called at all, so a zero-length read at a null pointer -- which is
            // legal C -- does not fault.
            return Ok(Settled::Done(0));
        }
""",
     """""",
     ANDROID),

    # Over-corrects: a write's SOURCE is admitted as writable. Demanding more than the operation
    # needs reads as stricter and refuses a guest writing out of its own `.rodata`, which is
    # ordinary.
    ("order-B2", "B", "a write's source buffer is admitted as writable",
     ADAPTER_FILES,
     """    let base = transfer_buffer(view, buf, count, false, 1)?;""",
     """    let base = transfer_buffer(view, buf, count, true, 1)?;""",
     ANDROID),

    # ---- The reason a stream failure gives the guest (M6). -----------------------------------
    #
    # `ferror` and `clearerr` ARE imported by `libroblox.so` and neither is bound, so the error
    # flag is write-only today: `errno` is the ENTIRE channel by which a guest learns *why* a
    # stream operation failed. That is what makes a missing one a silent wrong answer rather than
    # a missing convenience, and it is why these rows exist at all.
    #
    # Every "no errno" detector starts from a SENTINEL (`EDOM`, which nothing in this layer can
    # produce) rather than from zero. Asserting against 0 would pass for a layer that CLEARED
    # errno -- which POSIX forbids -- and would not see a stale value at all.

    ("errno-A1", "A", "fputc reports EOF without the errno that explains it",
     BIONIC_STDIO,
     """            // The descriptor's number, unchanged: POSIX.1-2017 XSH `fputc`'s ERRORS list is
            // `write()`'s, and only the layer that made the call can tell those apart. Dropping
            // this line is the defect this module's `errno` rule exists to close -- the guest
            // would still see `EOF`, and would read the reason for some earlier call.
            stream.error = true;
            ctx.set_errno(errno);
            EOF""",
     """            stream.error = true;
            EOF""",
     BIONIC),

    ("errno-A2", "A", "fputs and fwrite report a short count without the errno that explains it",
     BIONIC_STDIO,
     """                // The descriptor's number, unchanged: POSIX.1-2017 XSH `fputc` -- which is
                // `fputs`'s and `fwrite`'s ERRORS list by reference -- is `write()`'s, and only
                // the layer that made the call can tell `ENOSPC` from `EPIPE` from `EBADF`.
                stream.error = true;
                ctx.set_errno(errno);""",
     """                stream.error = true;""",
     BIONIC),

    ("errno-A3", "A", "fread reports a short count without the errno that explains it",
     BIONIC_STDIO,
     """                // The descriptor's number, unchanged: POSIX.1-2017 XSH `fread`'s ERRORS list is
                // `fgetc`'s, which is `read()`'s. Without this the guest gets the same short
                // count as an ordinary end of file, `feof` answers false, and `errno` explains
                // some earlier call.
                stream.error = true;
                ctx.set_errno(errno);""",
     """                stream.error = true;""",
     BIONIC),

    # Over-corrects. A zero-byte write for a non-empty buffer cannot happen on a device --
    # POSIX.1-2017 XSH `write()` transfers at least one byte or fails with errno set -- so there
    # is no true number for it, and `ENOSPC` is the most believable wrong one: it sends a guest
    # off deleting files to make room that was never the problem.
    ("errno-B1", "B", "a zero-byte write is given an invented ENOSPC",
     BIONIC_STDIO,
     """                stream.error = true;
                break;
            }
            Ok(took) => done += took as u64,""",
     """                stream.error = true;
                ctx.set_errno(consts::ENOSPC);
                break;
            }
            Ok(took) => done += took as u64,""",
     BIONIC),

    # The same over-correction one site along, with the number D23's refusal 7 already declines to
    # hand out.
    ("errno-B2", "B", "fputc's zero-byte write is given an invented EIO",
     BIONIC_STDIO,
     """            stream.error = true;
            EOF
        }
        Err(errno) => {""",
     """            stream.error = true;
            ctx.set_errno(consts::EIO);
            EOF
        }
        Err(errno) => {""",
     BIONIC),

    # Over-corrects in the adapter: the contract guard fires on an ORDINARY SHORT WRITE, refusing
    # a call that is doing exactly what `write()` is allowed to do. The guard is meant to catch
    # only `taken == 0` for a non-empty buffer.
    ("errno-B3", "B", "the adapter refuses an ordinary short write",
     ADAPTER_STDIO,
     """    if requested == 0 || taken > 0 {""",
     """    if requested == 0 || taken >= requested {""",
     ANDROID),

    # Over-corrects: reaching the end of a file is reported as a failure. C17 7.21.8.1p3 makes a
    # short count at the end of a file a return value rather than an error, and `feof` is what
    # tells it from the error arm.
    ("errno-B4", "B", "reaching the end of a file is reported as EIO",
     BIONIC_STDIO,
     """            // End of file: the flag, and **no `errno`**. C17 7.21.8.1p3 makes a short count at
            // the end of a file a return value rather than a failure, and `feof` is what tells
            // it from the error arm below. See the table on this function.
            Ok(0) => {
                stream.eof = true;""",
     """            Ok(0) => {
                ctx.set_errno(consts::EIO);
                stream.eof = true;""",
     BIONIC),

    # ---- The one lock-discipline site that is deterministically detectable (M6). --------------
    #
    # Eight sites were converted when `looper_in`/`window_in` closed the check-then-act window
    # (VERIFICATION entry 13). **Only this one gets rows.** The other seven -- `acquire`,
    # `release`, `removeFd`, `addFd`'s re-check and the window pair -- are behaviourally identical
    # single-threaded: reverting them changes nothing any single-threaded test can observe, and
    # reaching them needs a second guest thread inside a sleeping window, which is a sleep. Entry 6
    # is that a flake does not merely cost a red run -- it can make a row look detected when
    # nothing detected it. So those sites stay uncovered and labelled, not covered by something
    # that sometimes passes.

    ("lock-A1", "A", "pollOnce's callback arm asserts a liveness its own callback could have ended",
     NDK_LOOPER,
     """                    if let Some(entry) = state.loopers.get_mut(looper) {
                        entry.fds.retain(|held| held.fd != fd);
                    }""",
     """                    let entry = state.loopers.get_mut(looper).expect("the slot was checked live");
                    entry.fds.retain(|held| held.fd != fd);""",
     ANDROID),

    # The over-correction that reads as MORE careful: refuse, rather than treat a removal against a
    # looper the callback destroyed as the request already satisfied. It turns a guest sequence the
    # NDK documents -- release, then return 0 -- into a typed refusal, which ends the poll the glue
    # is driving. Caught on the ALOOPER_POLL_CALLBACK assertion, not on the registration set, which
    # is empty either way.
    ("lock-B1", "B", "a callback that destroyed its own looper is refused instead of answered",
     NDK_LOOPER,
     """                    if let Some(entry) = state.loopers.get_mut(looper) {
                        entry.fds.retain(|held| held.fd != fd);
                    }""",
     """                    let Some(entry) = state.loopers.get_mut(looper) else {
                        return Err(refuse_reentrant(
                            c,
                            format!(
                                "the callback for fd {fd} returned 0, but the looper at \
                                 {looper:#x} is no longer live"
                            ),
                        ));
                    };
                    entry.fds.retain(|held| held.fd != fd);""",
     ANDROID),

    # ---- W1: `__android_log_print` truncates where a device truncates (M6). ------------------
    #
    # Real liblog does not cap a formatted message after the fact -- it formats into a 1024-byte
    # stack buffer in the first place, and `vsnprintf` truncates rather than failing. This layer
    # refused instead, aborting the whole guest run for a log line: the exact outcome
    # `logging.rs`'s own module header says the module exists to prevent.
    #
    # The trap these rows exist for is `trunc-B1`. A wide field is not a long field that gets
    # shortened -- it is a field with PADDING ON ONE SIDE, and which side decides the bytes.
    # `%70000d` of 42 on a device yields 1023 spaces AND NO DIGITS AT ALL. Clamping the width to
    # the budget yields 1021 spaces then `42`: right length, right characters, wrong order, and a
    # tail no device has. It is byte-identical for the left-justified and zero-padded modes, so
    # it would have looked correct in two tests of three. Only an assertion on the bytes sees it.

    # the padding fill ignores the budget, so a guest-chosen width is a guest-chosen allocation
    # again Detector:
    # `a_wide_right_justified_field_is_cut_to_its_padding_and_never_to_its_number`.
    ("trunc-A2", "A", "the padding fill ignores the budget, so a guest-chosen width is a guest-chosen allocation again",
     BIONIC_PRINTF,
     """        let take = self.room().min(n);""",
     """        let take = n;""",
     BIONIC),

    # the text sink ignores the budget, so literals and bodies run past the buffer Detector:
    # `a_long_literal_run_truncates_under_a_budget`.
    ("trunc-A3", "A", "the text sink ignores the budget, so literals and bodies run past the buffer",
     BIONIC_PRINTF,
     """        let room = self.room();""",
     """        let room = usize::MAX;""",
     BIONIC),

    # the characters the budget dropped are not counted, so a cut line reports as whole Detector:
    # `a_wide_right_justified_field_is_cut_to_its_padding_and_never_to_its_number`.
    ("trunc-A4", "A", "the characters the budget dropped are not counted, so a cut line reports as whole",
     BIONIC_PRINTF,
     """        self.full = self.full.saturating_add(n);""",
     """        self.full = self.full.saturating_add(take);""",
     BIONIC),

    # the %s precision goes back to a byte slice: wrong unit, and a panic on a boundary a guest
    # picks Detector: `a_string_precision_counts_guest_bytes_and_cannot_split_a_character`.
    ("trunc-A5", "A", "the %s precision goes back to a byte slice: wrong unit, and a panic on a boundary a guest picks",
     BIONIC_PRINTF,
     """    match s.char_indices().nth(p) {
        Some((at, _)) => &s[..at],
        None => s,
    }""",
     """    &s[..p.min(s.len())]""",
     BIONIC),

    # the record discards the formatter's count, so a budget-cut line reads as complete Detector:
    # `a_message_the_formatter_cut_is_still_reported_as_truncated`.
    ("trunc-A6", "A", "the record discards the formatter's count, so a budget-cut line reads as complete",
     ADAPTER_LOGGING,
     """        record.truncated =
            Some(Truncation { tag_bytes: tag.len(), message_bytes: full_message_bytes });""",
     """        let _ = full_message_bytes;""",
     ANDROID_LIB),

    # the width is clamped to the budget, so a wide right-justified field ends with its number
    # where a device has only padding Detector:
    # `a_wide_right_justified_field_is_cut_to_its_padding_and_never_to_its_number`.
    ("trunc-B1", "B", "the width is clamped to the budget, so a wide right-justified field ends with its number where a device has only padding",
     BIONIC_PRINTF,
     """    let pad = width.unwrap_or(0).saturating_sub(body.len());""",
     """    let pad = width.unwrap_or(0).min(bound.budget).saturating_sub(body.len());""",
     BIONIC),

    # the unbounded entry point takes the total cap as a budget, so snprintf truncates where its
    # return value must be the full length Detector:
    # `a_repeated_wide_field_is_stopped_by_the_total_cap`.
    ("trunc-B2", "B", "the unbounded entry point takes the total cap as a budget, so snprintf truncates where its return value must be the full length",
     BIONIC_PRINTF,
     """    format_bounded(fmt, args, out, usize::MAX)?;""",
     """    format_bounded(fmt, args, out, MAX_OUTPUT)?;""",
     BIONIC),

    # the budget counts host bytes, so a message of guest bytes above 0x7F is cut at half what a
    # device keeps Detector: `the_budget_counts_guest_bytes_and_not_host_string_bytes`.
    ("trunc-B3", "B", "the budget counts host bytes, so a message of guest bytes above 0x7F is cut at half what a device keeps",
     BIONIC_PRINTF,
     """        let asked = text.chars().count();""",
     """        let asked = text.len();""",
     BIONIC),

    # the wide-precision refusal widens to every conversion, so %.70000d refuses where a device
    # truncates Detector:
    # `the_floating_conversions_this_engine_cannot_place_still_refuse_under_a_budget`.
    ("trunc-B4", "B", "the wide-precision refusal widens to every conversion, so %.70000d refuses where a device truncates",
     BIONIC_PRINTF,
     """        what == "precision" && matches!(conv, 'e' | 'E' | 'g' | 'G' | 'a' | 'A')""",
     '        what == "precision"',
     BIONIC),

    # the refusal is dropped for the arm this engine cannot place, so %.70000e emits digits that
    # are not vsnprintf's Detector:
    # `the_floating_conversions_this_engine_cannot_place_still_refuse_under_a_budget`.
    ("trunc-B5", "B", "the refusal is dropped for the arm this engine cannot place, so %.70000e emits digits that are not vsnprintf's",
     BIONIC_PRINTF,
     """        what == "precision" && matches!(conv, 'e' | 'E' | 'g' | 'G' | 'a' | 'A')""",
     """        let _ = (what, conv);
        false""",
     BIONIC),

    # the %f fill starts below the exact expansion, so a huge precision loses real digits to zeros
    # Detector: `a_huge_f_precision_is_a_fill_and_the_bytes_are_the_whole_conversions`.
    ("trunc-B6", "B", "the %f fill starts below the exact expansion, so a huge precision loses real digits to zeros",
     BIONIC_PRINTF,
     """pub const EXACT_FRACTION_DIGITS: usize = 1074;""",
     """pub const EXACT_FRACTION_DIGITS: usize = 16;""",
     BIONIC),

    # the log budget is cut below liblog's own buffer, shortening lines a device carries whole
    # Detector: `the_format_budget_is_liblogs_own_buffer`.
    ("trunc-B7", "B", "the log budget is cut below liblog's own buffer, shortening lines a device carries whole",
     ADAPTER_LOGGING,
     """const FORMAT_BUDGET: usize = MAX_MESSAGE_BYTES;""",
     """const FORMAT_BUDGET: usize = 64;""",
     ANDROID_LIB),

    # a record the formatter did not cut is reported as truncated Detector:
    # `a_message_the_formatter_did_not_cut_carries_no_truncation`.
    ("trunc-B8", "B", "a record the formatter did not cut is reported as truncated",
     ADAPTER_LOGGING,
     """    if full_message_bytes > message.len() {""",
     """    if full_message_bytes >= message.len() {""",
     ANDROID_LIB),

    # ---- M1's shape in the FILE * layer (M6). --------------------------------------------------
    #
    # `ff6719e` closed the finding in `read`/`pread`/`write`/`__write_chk`; the SAME shape was
    # still live one crate along, in `fread`, `fgets` and `fwrite`. `omni-bionic` cannot close it
    # itself: D19 leaves its `GuestMemory` with `read` and `write` and nothing else, so it has no
    # way to probe a mapping without writing to it, and a trait probe defaulted to `Ok(())` would
    # be a plausible stub for every implementer. The admission therefore lives in the adapter,
    # which has `checked_ptr` and a refusal channel -- the same placement, for the same two
    # reasons, as `7bf21af`'s zero-byte-write contract.
    #
    # `fwrite` is the SOURCE side and is admitted READABLE, not writable: `order-B2` is the row
    # that exists because demanding more than the operation needs refuses a guest writing out of
    # its own `.rodata`. `sorder-A5` is the `size` versus `size - 1` edge, which is worth its own
    # row because C writes at most `size - 1` bytes AND a terminator, so `size` is the admission.

    ("sorder-A1", "A", "fread consumes from the descriptor before the destination is admitted",
     ADAPTER_STDIO,
     """        if let Some(total) = size.checked_mul(nmemb) {
            admit_transfer(view, ptr, total, true, 0)?;
        }
        let produced = stdio::fread(view, descriptors, stream, ptr, size, nmemb);
""",
     """        let produced = stdio::fread(view, descriptors, stream, ptr, size, nmemb);
""",
     ANDROID),

    ("sorder-A2", "A", "fgets consumes from the descriptor before the destination is admitted",
     ADAPTER_STDIO,
     """        if size > 0 {
            // `size <= 0` is the case C17 7.21.7.2 does not define, and `omni_bionic::stdio`
            // answers it by reading nothing, writing nothing and setting no `errno`. There is no
            // transfer to admit, so admitting one would refuse a call that touches neither the
            // buffer nor the descriptor.
            admit_transfer(view, s, size as u64, true, 0)?;
        }
        let produced = stdio::fgets(view, descriptors, stream, s, size);
""",
     """        let produced = stdio::fgets(view, descriptors, stream, s, size);
""",
     ANDROID),

    ("sorder-A3", "A", "fwrite reaches the descriptor before the source is admitted",
     ADAPTER_STDIO,
     """        if let Some(total) = size.checked_mul(nmemb) {
            // As `fread`: an unrepresentable product is `EINVAL` from the layer below, not a
            // refusal about a pointer that was never the problem.
            admit_transfer(view, ptr, total, false, 0)?;
        }
        let produced = stdio::fwrite(view, descriptors, stream, ptr, size, nmemb);
""",
     """        let produced = stdio::fwrite(view, descriptors, stream, ptr, size, nmemb);
""",
     ANDROID),

    ("sorder-A4", "A", "only the first byte of a stream transfer buffer is admitted",
     ADAPTER_STDIO,
     """    view.mem().checked_ptr(at, len, write, Blame::new(view.symbol(), view.address(), argument))?;""",
     """    view.mem().checked_ptr(at, len.min(1), write, Blame::new(view.symbol(), view.address(), argument))?;""",
     ANDROID),

    ("sorder-A5", "A", "fgets admits size - 1, leaving the terminator's own byte unchecked",
     ADAPTER_STDIO,
     """            admit_transfer(view, s, size as u64, true, 0)?;""",
     """            admit_transfer(view, s, size as u64 - 1, true, 0)?;""",
     ANDROID),

    ("sorder-A6", "A", "the transfer loops walk the guest pointer with a wrapping add again",
     BIONIC_STDIO,
     """fn offset_from(base: u64, offset: u64) -> BionicResult<u64> {
    base.checked_add(offset).ok_or(BionicError::Memory(crate::memory::Fault(base)))
}""",
     """fn offset_from(base: u64, offset: u64) -> BionicResult<u64> {
    Ok(base.wrapping_add(offset))
}""",
     BIONIC),

    ("sorder-B1", "B", "a zero-length stream transfer is refused",
     ADAPTER_STDIO,
     """    if length == 0 {
        return Ok(());
    }
""",
     """""",
     ANDROID),

    ("sorder-B2", "B", "an fwrite's source buffer is admitted as writable",
     ADAPTER_STDIO,
     """            admit_transfer(view, ptr, total, false, 0)?;""",
     """            admit_transfer(view, ptr, total, true, 0)?;""",
     ANDROID),

    ("sorder-B3", "B", "an unrepresentable size * nmemb is refused instead of answered with EINVAL",
     ADAPTER_STDIO,
     """        if let Some(total) = size.checked_mul(nmemb) {
            admit_transfer(view, ptr, total, true, 0)?;
        }
""",
     """        admit_transfer(view, ptr, size.saturating_mul(nmemb), true, 0)?;
""",
     ANDROID),

    ("sorder-B4", "B", "fgets admits a buffer for a size C leaves undefined, so a legal no-op refuses",
     ADAPTER_STDIO,
     """        if size > 0 {
            // `size <= 0` is the case C17 7.21.7.2 does not define, and `omni_bionic::stdio`
            // answers it by reading nothing, writing nothing and setting no `errno`. There is no
            // transfer to admit, so admitting one would refuse a call that touches neither the
            // buffer nor the descriptor.
            admit_transfer(view, s, size as u64, true, 0)?;
        }
""",
     """        admit_transfer(view, s, size as u64, true, 0)?;
""",
     ANDROID),

    # ---- The null `jmethodID` on the game thread (M6). ----------------------------------------
    #
    # MEASURED: `CallObjectMethodV` was handed 0x0 at guest pc 0x02bd8cac, from a `GetMethodID`
    # at 0x02bdad0c asking `com/google/androidgamesdk/GameActivity` for
    # `getResources()Landroid/content/res/Resources;`. There is NO `cbz` between the two -- the
    # engine does not check. The member is declared on `android/content/Context` and the registry
    # had no superclass chain, so it answered null for a member the class genuinely has.
    #
    # The premise this was chased under was itself wrong and is worth recording: `Jni::misses` was
    # empty, which read as "the member IS declared". It was empty because `report()` ran the
    # instant step 13 returned -- BEFORE `android_main` had run at all. Measured after
    # `join_guest_threads`, misses = 1 and it named the member. **A census taken before the work
    # happens is not evidence that the work succeeded** (VERIFICATION entry 11, one layer up).

    # The defect the gate found for real: `getResources()` is declared on `Context` and asked of
    # `GameActivity`, so a flat registry answers null and the guest hands that null straight to
    # `CallObjectMethodV` with no `cbz` in between -- section 8.1 failure mode 3, at guest pc
    # 0x02bdad0c.
    ("jmid-A1", "A", "the member lookup stops walking the superclass chain",
     JNI_CLASSES,
     """        self.ancestry(class).find_map(|at| self.declared_method(at, name, descriptor, is_static))""",
     """        self.declared_method(class, name, descriptor, is_static)""",
     ANDROID_LIB),
    ("jmid-A2", "A", "the superclass edges are resolved and then thrown away",
     JNI_CLASSES,
     """            self.classes[usize::from(sub.0)].superclass = Some(sup);""",
     """            self.classes[usize::from(sub.0)].superclass = None;""",
     ANDROID_LIB),

    # This edge is the one that cannot be recovered by inspection: `MainGameActivity`'s name
    # appears in ZERO .rodata string literals, because the engine only ever reaches the class
    # through `GetObjectClass`. A registry that learned its edges from `FindClass` arguments would
    # never see it.
    ("jmid-A3", "A", "the MainGameActivity edge -- the one no FindClass can reveal -- is dropped",
     JNI_CLASSES,
     """    ("com/roblox/client/startup/MainGameActivity", "com/google/androidgamesdk/GameActivity"),""",
     """""",
     GATE_ACTIVITY),
    ("jmid-A4", "A", "the gate calls step 13 on a bare GameActivity again",
     GATE_ACTIVITY_FILE,
     """const ACTIVITY_CLASS: &str = "com/roblox/client/startup/MainGameActivity";""",
     """const ACTIVITY_CLASS: &str = "com/google/androidgamesdk/GameActivity";""",
     GATE_ACTIVITY),
    ("jmid-A5", "A", "a static call's receiver is reported as java/lang/Class instead of itself",
     JNI_ENV,
     """                Some(Object::Class(class)) => {""",
     """                Some(Object::Class(class)) if false => {""",
     ANDROID_LIB),

    # The generous answer, which is worse than the null. Answering from ANY class that happens to
    # declare the member makes a wrong receiver into a working call, and the mistake surfaces as
    # wrong behaviour thousands of instructions later rather than as a refusal here.
    ("jmid-B1", "B", "the lookup falls back to any class that declares the member",
     JNI_CLASSES,
     """        self.ancestry(class).find_map(|at| self.declared_method(at, name, descriptor, is_static))""",
     """        self.ancestry(class)
            .find_map(|at| self.declared_method(at, name, descriptor, is_static))
            .or_else(|| {
                (0..self.classes.len()).find_map(|at| {
                    self.declared_method(ClassId(at as u16), name, descriptor, is_static)
                })
            })""",
     GATE_ACTIVITY),

    # The tempting consistency. `declared_method` is what `RegisterNatives` and the
    # generated-surface merge use, and both mean "declared on THIS class": making it walk would
    # let a native registered on a superclass satisfy a subclass binding.
    ("jmid-B2", "B", "declared_method walks the chain too, superclass first",
     JNI_CLASSES,
     """    pub fn declared_method(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<MethodId> {
        let declared = self.class(class)?;""",
     """    pub fn declared_method(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<MethodId> {
        if let Some(found) = self
            .class(class)?
            .superclass
            .and_then(|sup| self.declared_method(sup, name, descriptor, is_static))
        {
            return Some(found);
        }
        let declared = self.class(class)?;""",
     ANDROID_LIB),

    # ---- FMOD's `checkInit` and `supportsAAudio`: an answer that follows a step ------------------
    #
    # MEASURED, gate run 62: guest thread 6 died on `org/fmod/FMOD.checkInit()Z` (called from
    # `0x4fc0284`, FMOD's first JNI call) and presents stopped. `checkInit` is `gContext != null`;
    # `NativeHelper.Q` sets `gContext` with `FMOD.init(this.a)` at `0x0023`, between two of step 11's
    # downcalls. So the answer is a function of whether that step ran -- which is exactly what a
    # constant would stop being. The B rows are the constants that read as right.

    # **The defect the gate found, put back**: the refusal that killed the render thread.
    ("fmod-A1", "A", "FMOD.checkInit refuses again, as it did at gate run 62",
     JNI_CLASSES,
     """            s("checkInit", "()Z", Answer::StaticIsSet("gContext")),""",
     """            s("checkInit", "()Z", Answer::Unanswered),""",
     ANDROID_LIB),
    ("fmod-A2", "A", "the script no longer performs FMOD.init at NativeHelper.Q's row",
     JNI_SCRIPT,
     """        java_before: &[FMOD_INIT],""",
     """        java_before: &[],""",
     ANDROID_LIB),
    ("fmod-A3", "A", "run skips every row's Java statements",
     JNI_SCRIPT,
     """        if let Err(error) = perform(jni, step.java_before) {""",
     """        if let Err(error) = perform(jni, &[]) {""",
     ANDROID_LIB),
    ("fmod-A4", "A", "the Java store anchors the object and never records it in the static",
     JNI_MOD,
     """            Some(anchor) => state.statics.insert(found, anchor),""",
     """            Some(_anchor) => None,""",
     ANDROID_LIB),
    ("fmod-A5", "A", "a static read no longer consults what a Java statement assigned",
     JNI_ENV,
     """        Answer::Assigned => assigned(state, name, address, field, member),
""",
     """""",
     ANDROID_LIB),

    # **The device's answer, hard-coded.** True on every device that ran step 11 -- and true here
    # whether or not anything ran it, which is the claim this layer must not make.
    ("fmod-B1", "B", "FMOD.checkInit hard-coded to the device's `true`",
     JNI_CLASSES,
     """            s("checkInit", "()Z", Answer::StaticIsSet("gContext")),""",
     """            s("checkInit", "()Z", Answer::Bool(true)),""",
     ANDROID_LIB),
    # **"There is no AAudio here, so say so" -- in the wrong place.** `supportsAAudio` is
    # `SDK_INT >= 27`, true on the Android 13 this host presents; the absence of `libaaudio.so` is
    # answered by `dlopen`, where it is a fact. `false` here would send FMOD down the
    # `supportsLowLatency` branch on a claim about Android that is untrue.
    ("fmod-B2", "B", "FMOD.supportsAAudio answers false because this host has no libaaudio.so",
     JNI_CLASSES,
     """                Answer::Bool(super::script::ANDROID_SDK_LEVEL >= FMOD_AAUDIO_MIN_SDK),""",
     """                Answer::Bool(false),""",
     ANDROID_LIB),
    ("fmod-B3", "B", "a Java-assigned static takes any object, whatever its declared type",
     JNI_MOD,
     """            if !admits {""",
     """            if false && !admits {""",
     ANDROID_LIB),
    # "As early as possible" -- FMOD.init claimed to have run at step 7, before the Java that runs
    # it. Nothing observable breaks in a run; the claim about when is false.
    ("fmod-B4", "B", "FMOD.init is also performed at the first scripted row, step 7",
     JNI_SCRIPT,
     """        class: "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        java_before: &[],""",
     """        class: "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
        member: "init",
        descriptor: "(Landroid/content/Context;)V",
        java_before: &[FMOD_INIT],""",
     ANDROID_LIB),
    ("fmod-B5", "B", "checkInit answers whether gContext is declared rather than whether it is set",
     JNI_ENV,
     """            Ok(Value::Boolean(matches!(value, Value::Object(Some(_)))))""",
     """            let _ = value;
            Ok(Value::Boolean(true))""",
     ANDROID_LIB),
    # **A plausible number for a static the decoded path never reaches**: FMOD would size its mixer
    # on a sample rate nothing measured. Only the OpenSL ES output asks for it (`0x4fbd438`).
    ("fmod-B6", "B", "FMOD.getOutputSampleRate answered with a plausible 48000",
     JNI_CLASSES,
     """                Answer::Bool(super::script::ANDROID_SDK_LEVEL >= FMOD_AAUDIO_MIN_SDK),
            ),
        ],""",
     """                Answer::Bool(super::script::ANDROID_SDK_LEVEL >= FMOD_AAUDIO_MIN_SDK),
            ),
            s("getOutputSampleRate", "()I", Answer::Int(48000)),
        ],""",
     ANDROID_LIB),

    # ======================================= M6: the indefinite `ALooper_pollOnce`, and the fact
    # ======================================= it is decided on
    #
    # §8 rows 17-20 need `ALooper_pollOnce(-1)` to *wait*, and M5 refused it: a host thread parked
    # on a descriptor nobody can write to cannot be ended by a step budget, because a sleeping
    # thread executes no guest instructions (D16). What changed is not the policy but the
    # **measurement** -- `Filesystem::pipe_writers` counts the live write ends, so "something can
    # still make this ready" is a fact this runtime holds rather than a hope. Every row below is a
    # way for that fact to be wrong while every count-based assertion around it still passes.

    # The wake-source filter admitting a count of zero. A read end whose last writer has closed is
    # *readable* -- end of file is a read that returns immediately -- so the refusal cannot rest on
    # readiness, and `> 0` is the entire difference between a measured fact and a restated hope.
    ("looper-A9", "A", "a pipe with no live writer counts as a wake source",
     NDK_LOOPER,
     """                    .filter(|writers| *writers > 0)""",
     """                    .filter(|writers| *writers >= 0)""",
     ANDROID),

    # The decision inverted rather than removed, which is the shape that catches **both** arms with
    # one row: the dead-pipe case stops being refused and the live-pipe case starts being. A row
    # that only deleted the refusal would leave the arm M6 turned on untested.
    ("looper-A10", "A", "the indefinite-wait check answers the opposite question",
     NDK_LOOPER,
     """        if sources.is_empty() {""",
     """        if !sources.is_empty() {""",
     ANDROID),

    # **The ordering, as a mutation.** The stop switch has to be read below the deadline test,
    # because `pollOnce(0)` is a poll and not a wait: it asks what is ready now, and POLL_TIMEOUT is
    # the true answer when nothing is. This row is the check moved back above it -- written as an
    # equivalent early check rather than as a hunk that moves the block, so the `old` stays one
    # line and cannot stale against the paragraph of comment that sits between the two.
    ("looper-A11", "A", "the stop switch read above the deadline test, so a zero-timeout poll refuses",
     NDK_LOOPER,
     """        let slice = match bound {""",
     """        if bionic.bionic.guest_threads_stopping() {
            return Err(refuse_reentrant(
                c,
                "the stop switch was read above the deadline test".to_string(),
            ));
        }
        let slice = match bound {""",
     ANDROID),

    # The indefinite arm answering "no time left" instead of a slice, so an indefinite poll returns
    # POLL_TIMEOUT at once -- a timeout reported to a call that was given none, which is the exact
    # answer the refusal above exists to avoid producing.
    #
    # There is deliberately **no row for `WAIT_SLICE`'s value**, and the reason is a finding rather
    # than an omission: the readiness gate is a condvar, so a write wakes the wait whatever the
    # slice is, and no test asserts the stop switch ends an indefinite park. A slice of an hour
    # therefore passes every test in `tests/ndk.rs` today, and a row that cannot fail is worse than
    # no row (Task 1). What would close it is a test that stops a runtime out of an indefinite
    # `pollOnce` and bounds how long that takes.
    ("looper-A12", "A", "an indefinite poll times out at once instead of waiting",
     NDK_LOOPER,
     """            Bound::Indefinite => WAIT_SLICE,""",
     """            Bound::Indefinite => break Pass::Idle,""",
     ANDROID),

    # ---- the fact itself, at the seam ----

    # The count substituted for the other end's. **A substitution, not a count** (entry 1): the
    # number is still a number of descriptors, `describe_watched` still prints "N live writer(s)",
    # and a read end whose writers have all closed reports one because its *reader* is open.
    ("pipe-A9", "A", "the live-writer count answers with the readers instead",
     PLAT_FS_PIPE,
     """    pub fn writers(&self) -> usize {
        self.state().writers
    }""",
     """    pub fn writers(&self) -> usize {
        self.state().readers
    }""",
     ANDROID),

    # The fact made unavailable: a pipe answers `None`, which is what a descriptor that is not a
    # pipe answers. Every indefinite wait is then refused again -- the M5 behaviour, which is the
    # believable wrong answer because it reads as conservative.
    ("pipe-A10", "A", "a pipe reports no writer count, so every indefinite wait is refused again",
     PLAT_FS,
     """            Some(Entry::Pipe(handle)) => Some(handle.pipe().writers()),""",
     """            Some(Entry::Pipe(handle)) => {
                let _ = handle;
                None
            }""",
     ANDROID),

    # The over-correction: a second descriptor kind answering the question too. An eventfd really
    # can be written, so "it has a writer" reads as true -- and it is not the question. There is no
    # *count of ends* for an eventfd, so an indefinite wait would be admitted on a descriptor
    # nothing in this runtime is holding open to write to.
    ("pipe-B5", "B", "a kind that is not a pipe answers the live-writer count as well",
     PLAT_FS,
     """            Some(Entry::Pipe(handle)) => Some(handle.pipe().writers()),""",
     """            Some(Entry::Pipe(handle)) => Some(handle.pipe().writers()),
            Some(Entry::EventFd(_)) => Some(1),""",
     ANDROID),

    # ======================================= M6: which clock a `pthread_cond_timedwait` is absolute in
    #
    # `cond::clock_of` reads back a selector **this crate wrote**, in a field this crate defined at
    # `cond + 4`, because there is no bionic source on this host to check a bit layout against
    # (entry 10). So these rows are about the convention being read back exactly, which is the only
    # thing that makes the round trip evidence of anything.

    # The field offset. An all-zero `pthread_cond_t` is `PTHREAD_COND_INITIALIZER` and must answer
    # CLOCK_REALTIME, so reading the *wrong* word still answers a legal clock -- it answers
    # REALTIME for every cond ever created, including the ones that asked for MONOTONIC.
    ("cond-A1", "A", "the clock selector read from the wrong word of the cond",
     BIONIC_COND,
     """    mem.read(cond_addr + 4, &mut b)?;""",
     """    mem.read(cond_addr, &mut b)?;""",
     BIONIC),

    # The monotonic arm answering the other clock. On this host the two differ by decades, and a
    # `timedwait` given a monotonic deadline measured against the wall clock waits for ever or not
    # at all -- neither of which is an error anything reports.
    ("cond-A2", "A", "a cond initialised for CLOCK_MONOTONIC reports CLOCK_REALTIME",
     BIONIC_COND,
     """        clock_sel::MONOTONIC => Ok(clock_id::CLOCK_MONOTONIC),""",
     """        clock_sel::MONOTONIC => Ok(clock_id::CLOCK_REALTIME),""",
     BIONIC),

    # The refusal for a selector `init` never writes, turned into a clock picked by falling
    # through. The struct is then not one this layer produced and is treated as though it were,
    # which is entry 12's shape in reverse: a check that reads as defensive doing nothing.
    ("cond-A3", "A", "an unknown clock selector falls through to a clock instead of EINVAL",
     BIONIC_COND,
     """        _ => Err(consts::EINVAL),""",
     """        _ => Ok(clock_id::CLOCK_REALTIME),""",
     BIONIC),

    # ======================================= M6: the scheduling band the engine sizes itself against
    #
    # MEASURED at guest `0x054e0260`/`0x054e026c`: `libroblox.so` takes the min and the max for
    # SCHED_FIFO, rejects -1 from either, and requires `max - min >= 3`. Both rows below keep that
    # relation true, so `the_real_time_band_is_wide_enough_for_the_engine_check_at_0x054e0280`
    # passes for both of them -- they are caught by the test that names every value instead. That
    # pairing is the whole point of having the two tests (entry 1).
    ("sched-A1", "A", "the real-time maximum is a number of this layer's own rather than Linux's 99",
     BIONIC_METADATA,
     """        sched_policy::FIFO | sched_policy::RR => 99,""",
     """        sched_policy::FIFO | sched_policy::RR => 32,""",
     BIONIC),

    ("sched-A2", "A", "the real-time band starts at 0, where the time-sharing policies do",
     BIONIC_METADATA,
     """        sched_policy::FIFO | sched_policy::RR => 1,""",
     """        sched_policy::FIFO | sched_policy::RR => 0,""",
     BIONIC),

    # The policy **set**, which is the other half and the one a band check cannot see: SCHED_OTHER
    # and SCHED_BATCH drop out of the zero arm and answer -1, the value that means "Linux does not
    # define this policy". A caller is then told its own default policy does not exist.
    ("sched-A3", "A", "sched_get_priority_max stops defining SCHED_OTHER and SCHED_BATCH",
     BIONIC_METADATA,
     """        sched_policy::FIFO | sched_policy::RR => 99,
        sched_policy::OTHER | sched_policy::BATCH | sched_policy::IDLE => 0,""",
     """        sched_policy::FIFO | sched_policy::RR => 99,
        sched_policy::IDLE => 0,""",
     BIONIC),

    ("sched-A4", "A", "sched_get_priority_min stops defining SCHED_OTHER and SCHED_BATCH",
     BIONIC_METADATA,
     """        sched_policy::FIFO | sched_policy::RR => 1,
        sched_policy::OTHER | sched_policy::BATCH | sched_policy::IDLE => 0,""",
     """        sched_policy::FIFO | sched_policy::RR => 1,
        sched_policy::IDLE => 0,""",
     BIONIC),

    # ======================================= M6: `pthread_cond_timedwait`'s absolute deadline
    #
    # The whole of what this handler adds over `pthread_cond_wait` is turning an absolute
    # `timespec` into a relative wait. Each row below is a way to get a *plausible* duration out of
    # that arithmetic rather than an error.

    # The subtraction reversed, which is the sign error the handler's own comment is about: a
    # deadline in the past becomes a wait of however long ago it was. With the epoch as the
    # deadline that is ~57 years, so it trips the cap and the past-deadline test gets a refusal
    # where it expected ETIMEDOUT -- it fails at once rather than by waiting.
    ("timedwait-A1", "A", "the absolute deadline subtracted the wrong way round",
     ADAPTER_HANDLERS,
     """    let budget = absolute.saturating_sub(now);""",
     """    let budget = now.saturating_sub(absolute);""",
     ANDROID),

    # bionic's own validation of `tv_nsec` removed. A nanosecond field of exactly one billion then
    # carries into the seconds, and a negative one becomes zero through the `try_from`, so both
    # produce a wait for a time the caller never expressed instead of the EINVAL a device answers.
    ("timedwait-A2", "A", "an out-of-range tv_nsec becomes a wait instead of EINVAL",
     ADAPTER_HANDLERS,
     """    if !(0..1_000_000_000).contains(&nanos) {""",
     """    if false {""",
     ANDROID),

    # The cap removed, so a guest-chosen absolute deadline is waited out in full.
    #
    # **This row fails slowly and that is inherent**: the detector asks for twice the cap, so the
    # mutated handler waits the ~120 s it was given and then answers ETIMEDOUT where a refusal was
    # expected. It is a bounded failure rather than `pipe-B2`'s hang -- an absolute deadline is
    # finite by construction -- but it is worth knowing before reading the elapsed column.
    ("timedwait-A3", "A", "the guest-chosen wait cap removed, so a deadline is waited out in full",
     ADAPTER_HANDLERS,
     """    if budget.as_secs() > super::MAX_SLEEP_SECONDS {""",
     """    if false {""",
     ANDROID),

    # There is deliberately **no row for the clock selection** -- reading `now` from the monotonic
    # clock for a cond that asked for CLOCK_REALTIME. It is inert against every test that exists:
    # no test builds a cond on CLOCK_MONOTONIC and waits on it, and both realtime detectors survive
    # the substitution (the past-deadline one still sees a deadline in the past, and the cap one
    # still sees a deadline past the cap, because the two clocks differ by far more than the cap).
    # A row would MISS, and a row that cannot fail is worse than no row. What would close it is a
    # test that sets `pthread_condattr_setclock(CLOCK_MONOTONIC)` and asserts the wait's length.

    # ======================================= M6: the eventfd counter (§8 row 21)
    #
    # The first descriptor kind whose *read* changes what the next read answers, which is why these
    # rows are mostly about the second read rather than the first.

    # The destructive read made non-destructive. The counter keeps its value, so a second read
    # answers 7 again -- every assertion about the *first* read still passes, and a guest using the
    # eventfd as a wakeup token never stops being woken.
    ("eventfd-A1", "A", "an eventfd read does not consume the counter",
     PLAT_FS_EVENTFD,
     """                std::mem::replace(&mut *count, 0)""",
     """                *count""",
     ANDROID),

    # A read of a zero counter succeeding instead of reporting WouldBlock. It delivers a zero,
    # which is a value no write produced and which the guest cannot tell from a real one.
    ("eventfd-A2", "A", "a read of a zero counter delivers a zero rather than blocking",
     PLAT_FS_EVENTFD,
     """            if *count == 0 {""",
     """            if false {""",
     ANDROID),

    # Readiness losing its dependence on the counter: the always-ready answer the other kinds get.
    # `poll` then reports an eventfd readable while its counter is zero, and the read that follows
    # blocks or reports EAGAIN -- the readiness table contradicting the operation it describes.
    ("eventfd-A3", "A", "an eventfd is readable whether or not it has been written",
     PLAT_FS_EVENTFD,
     """            readable: count > 0,""",
     """            readable: true,""",
     ANDROID),

    # The flag check at the seam. `EFD_SEMAPHORE`, `EFD_CLOEXEC` and `EFD_NONBLOCK` are the whole of
    # what `eventfd2` defines, and the bit most likely to be smuggled in is one that changes
    # blocking -- the single decision this seam refuses to fake.
    ("eventfd-A4", "A", "a flag eventfd2 does not define is accepted rather than EINVAL",
     PLAT_FS,
     """        let unknown = flags & !eventfd::KNOWN_FLAGS;
        if unknown != 0 {""",
     """        let unknown = flags & !eventfd::KNOWN_FLAGS;
        if false {""",
     ANDROID),

    # The over-correction, and the copy-paste that reads as symmetry: writability given the
    # readability rule. An empty eventfd is then reported as unwritable, so a poller waiting to
    # *post* a token waits for a condition that only its own posting could produce.
    ("eventfd-B1", "B", "writability answers the readability question, so an empty eventfd is unwritable",
     PLAT_FS_EVENTFD,
     """            writable: count < MAX_COUNT,""",
     """            writable: count > 0,""",
     ANDROID),

    # There is deliberately no row for `MAX_COUNT`, for the eight-byte rule on either transfer, or
    # for the `0xffffffffffffffff` write refusal. All three are **inert against every test that
    # exists**: nothing performs a short read or write, nothing writes `u64::MAX`, and nothing
    # takes the counter near its ceiling, so each would report a MISS that read as a missing test
    # rather than as a missing case. What would close them is one test that does each of those
    # three things; they are cheap, and they are not written yet.

    # ======================================= M6: the A64 hint instructions (§8 rows 21-22)
    #
    # MEASURED on this pin: `hook_hint_instructions: 0` is not forwarded by the x64 A64 backend, so
    # a `yield` arrives at `handle_exception` as an exception with the config asking for the
    # opposite. Treating it as unsupported halts a guest whose spin loop is three instructions
    # long, which is what made this reachable.
    ("cpu-A40", "A", "a hint is no longer recognised as one, so every hint halts the guest",
     CPU_CALLBACKS,
     """const fn is_hint(kind: u32) -> bool {
    matches!(
        kind,
        exception::YIELD
            | exception::WAIT_FOR_EVENT
            | exception::WAIT_FOR_INTERRUPT
            | exception::SEND_EVENT
            | exception::SEND_EVENT_LOCAL
    )
}""",
     """const fn is_hint(_kind: u32) -> bool {
    false
}""",
     CPU),

    # One member dropped from the set rather than the set removed. `YIELD` is the one the measured
    # spin loop at guest `0x021eba20` emits, and the other four still passing is what makes this
    # the flattering direction: four of five hints work.
    ("cpu-A41", "A", "YIELD drops out of the hint set while the other four stay",
     CPU_CALLBACKS,
     """        exception::YIELD
            | exception::WAIT_FOR_EVENT
            | exception::WAIT_FOR_INTERRUPT
            | exception::SEND_EVENT
            | exception::SEND_EVENT_LOCAL""",
     """        exception::WAIT_FOR_EVENT
            | exception::WAIT_FOR_INTERRUPT
            | exception::SEND_EVENT
            | exception::SEND_EVENT_LOCAL""",
     CPU),

    # The early return removed, so a hint is counted, yields the host thread, and *then* falls
    # through to the unsupported path anyway. The `hints` counter rises while the guest stops --
    # entry 11's shape exactly, a watch that looks like a detector.
    ("cpu-A42", "A", "a hint is counted and then halts the guest as an unsupported instruction",
     CPU_CALLBACKS,
     """                return;
            }
            let address = pc as GuestAddr;""",
     """            }
            let address = pc as GuestAddr;""",
     CPU),

    # ======================================= M6: the two symbols a guest WORKER THREAD died on
    #
    # `pthread_getattr_np` and `pthread_mutex_trylock`, both found by `guest_thread_failures`
    # rather than by a downcall stopping. The first needed new logic -- it is the first member of
    # the attr family that reports a LIVE thread, so `pthread_create` had to start recording
    # where it puts a stack -- and the second needed only its binding, because
    # `omni_bionic::mutex::trylock` has been written and unit-tested since phase 3c.
    #
    # Every row below keeps the answer *plausible*, which is the only kind worth writing here: a
    # stack base one guard page out is a real, readable, writable address in the right mapping,
    # and nothing a caller can check would notice it.

    # The guard page reported as part of the stack. The base then names a PROT_NONE page, so the
    # thread that writes to the byte it was told is its lowest faults -- and a garbage collector
    # scanning from it would fault on the first word.
    ("worker-A1", "A", "the reported stack base is the mapping's, not the first byte above the guard",
     ADAPTER_THREADS,
     """            base: stack_base + guard,""",
     """            base: stack_base,""",
     ANDROID),

    # The same error from the other end: the size grown by the guard instead of the base moved.
    # `base + size` then runs one page past the top of the mapping, which is the direction that
    # makes a "how much stack is left" calculation too generous rather than too small.
    ("worker-A2", "A", "the guard is folded into the reported stack size",
     ADAPTER_THREADS,
     """            size: stack_bytes,
            guard,""",
     """            size: stack_bytes + guard,
            guard,""",
     ANDROID),

    # The identity test dropped, so every question is answered from the CALLING thread's record.
    # An unknown `pthread_t` stops being ESRCH and a known one gets somebody else's 256 KiB --
    # the one wrong answer a caller cannot detect, because the range it is handed is real.
    ("worker-A3", "A", "pthread_getattr_np answers any pthread_t from the calling thread's own stack",
     ADAPTER_THREADS,
     """    if thid != me.0 {""",
     """    if thid != me.0 && thid == u64::MAX {""",
     ANDROID),

    # The detach state guessed rather than read from the instance's record. JOINABLE is the
    # flattering direction: it is what most threads are, and the one it is wrong for is the one
    # nobody may join.
    ("worker-A4", "A", "the detach state is assumed joinable instead of read from the live record",
     ADAPTER_THREADS,
     """    let detached = v
        .active
        .bionic
        .guest_thread_list()
        .into_iter()
        .find(|summary| summary.id == me)
        .map(|summary| summary.detached);""",
     """    let detached = Some(false);""",
     ANDROID),

    # The measured M6 failure itself, as a row: the symbol is not bound, so the guest's call
    # reaches `Binding::Unbound` and the worker thread that made it dies. `thread: 3`,
    # `start_routine: 0x284d168`, at image offset 0x2b53aa0.
    ("worker-A5", "A", "pthread_mutex_trylock is not bound at all, which is the M6 thread failure",
     ADAPTER_HANDLERS,
     """    ("pthread_mutex_trylock", pthread_mutex_trylock),""",
     """""",
     ANDROID),

    # A second `pthread_attr_t` layout, which is what this crate's round-trip test exists to
    # stop: the stack base written where `attr_setguardsize` puts the guard. The attr is
    # self-consistent and wrong, and a guest reading it with `pthread_attr_getstack` is told its
    # stack starts at zero -- the "system default" encoding -- while its guard is an address.
    ("worker-A6", "A", "the live attr's stack base is written over the guard-size field",
     BIONIC_METADATA,
     """    mem.write(attr_addr + 24, &stack_base.to_le_bytes())?;""",
     """    mem.write(attr_addr + 16, &stack_base.to_le_bytes())?;""",
     BIONIC),

    # The zeroing dropped, with the range check kept so that only the erasure changes. The
    # reachable guest pattern is one attr used for `pthread_attr_init` + `setstacksize` +
    # `pthread_create` and then for `pthread_getattr_np`, so the fields this call does not reach
    # keep the earlier REQUEST -- and "how big is my stack" answers "as big as you asked for".
    ("worker-A7", "A", "a live attr is written over whatever the caller left in the object",
     BIONIC_METADATA,
     """    attr_init(mem, attr_addr)?;""",
     """    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;""",
     BIONIC),

    # The detach state constant-folded in the crate rather than in the adapter. Same wrong
    # answer as `worker-A4` and a different place to make it, which is why both are written: one
    # is caught by the guest-level test and one by the unit test beside the function.
    ("worker-A8", "A", "the live attr always reports JOINABLE whatever it was given",
     BIONIC_METADATA,
     """    let state = if detached { detach_state::DETACHED } else { detach_state::JOINABLE };""",
     """    let state = detach_state::JOINABLE;""",
     BIONIC),

    # The over-correction: refusing a question POSIX has an answer for. ESRCH is a value guest
    # code branches on -- it is how a caller discovers the thread it held a `pthread_t` for has
    # gone -- and a refusal there stops a correct guest over a case it had already handled.
    ("worker-B1", "B", "an unknown pthread_t is refused by name instead of answered ESRCH",
     ADAPTER_THREADS,
     """        if !known {
            return Ok(consts::ESRCH);
        }""",
     """        if !known {
            return Err(v.refusal("this runtime does not model another thread's attributes"));
        }""",
     ANDROID),
    # ---- M6: the socket surface and the `struct addrinfo` marshalling ---------------------------
    #
    # D30 withdrew Global Constraint 8 and this is the half of it that is bytes rather than policy.
    # Every row below is a change that would be **silently** wrong: the guest would get a list it
    # could walk, a `sockaddr` it could pass to `connect`, and an `EAI_*` code it has a branch for
    # -- and each would be about the wrong address, the wrong port or the wrong failure. None of
    # them is a crash, which is why they are here.

    ("sock-A1", "A", "sin_port written in the guest's byte order instead of network order",
     ADAPTER_ADDRINFO,
     """            out[2..4].copy_from_slice(&port.to_be_bytes());
            out[4..8].copy_from_slice(address);""",
     """            out[2..4].copy_from_slice(&port.to_le_bytes());
            out[4..8].copy_from_slice(address);""",
     ANDROID),

    ("sock-A2", "A", "sin6_flowinfo byte-swapped: a host-order field written network order",
     ADAPTER_ADDRINFO,
     """            out[SIN6_FLOWINFO_OFFSET..SIN6_FLOWINFO_OFFSET + 4]
                .copy_from_slice(&flowinfo.to_le_bytes());""",
     """            out[SIN6_FLOWINFO_OFFSET..SIN6_FLOWINFO_OFFSET + 4]
                .copy_from_slice(&flowinfo.to_be_bytes());""",
     ANDROID),

    # The one the layout warning is about. `ai_canonname` and `ai_addr` have the same type and the
    # structure has the same `sizeof` either way, so nothing about a size or a shape notices.
    ("sock-A3", "A", "ai_addr written at glibc's offset instead of bionic's",
     ADAPTER_ADDRINFO,
     """        out[field(AI_ADDR_OFFSET)..field(AI_ADDR_OFFSET) + 8]
            .copy_from_slice(&sockaddr_address.to_le_bytes());""",
     """        out[field(AI_CANONNAME_OFFSET)..field(AI_CANONNAME_OFFSET) + 8]
            .copy_from_slice(&sockaddr_address.to_le_bytes());""",
     ANDROID),

    ("sock-A4", "A", "ai_addrlen reports the slab's slot size rather than the family's",
     ADAPTER_ADDRINFO,
     """        out[field(AI_ADDRLEN_OFFSET)..field(AI_ADDRLEN_OFFSET) + 4]
            .copy_from_slice(&(addrlen as u32).to_le_bytes());""",
     """        out[field(AI_ADDRLEN_OFFSET)..field(AI_ADDRLEN_OFFSET) + 4]
            .copy_from_slice(&(SOCKADDR_SLOT_BYTES as u32).to_le_bytes());""",
     ANDROID),

    # A list that never terminates. The guest walks `ai_next` until it is null, so this is a walk
    # into the rest of the slab -- and the nodes there are zeroed, so it reads as an address of
    # 0.0.0.0 rather than as a fault.
    ("sock-A5", "A", "ai_next points past the last node instead of ending the list",
     ADAPTER_ADDRINFO,
     """        let next = if index + 1 < nodes.len() {
            (at + (index + 1) * ADDRINFO_BYTES) as u64
        } else {
            0
        };""",
     """        let next = (at + (index + 1) * ADDRINFO_BYTES) as u64;""",
     ANDROID),

    # The free list matched by range rather than by the exact head. A guest that walks and frees
    # as it goes passes `res->ai_next`, and this would release a slot it is still reading.
    ("sock-A6", "A", "freeaddrinfo matches any pointer into the slot instead of the head",
     ADAPTER_ADDRINFO,
     """        match live.iter().position(|slot| *slot == Some(head)) {""",
     """        match live.iter().position(|slot| {
            slot.is_some_and(|at| head >= at && head < at + ADDRINFO_RESULT_BYTES)
        }) {""",
     ANDROID),

    ("sock-A7", "A", "a full slab reuses a live slot instead of refusing",
     ADAPTER_ADDRINFO,
     """        let index = live.iter().position(Option::is_none)?;""",
     """        let index = live.iter().position(Option::is_none).unwrap_or(0);""",
     ANDROID),

    # D30 names this one as the trap in as many words: an `EAI_*` for a failure nobody classified
    # tells the guest the name does not exist, which is permanent, so it stops asking for ever.
    ("sock-A8", "A", "an unclassified resolver failure is given EAI_NONAME instead of refusing",
     ADAPTER_NET,
     """        // `ResolveFailure::Unclassified`, and nothing else. A wildcard because the enum is
        // `#[non_exhaustive]`: a class added upstream without a decision here must refuse by name
        // rather than acquire a plausible code.
        _ => return None,""",
     """        _ => net::EAI_NONAME,""",
     ANDROID),

    ("sock-A9", "A", "a guest sockaddr's port is read in the guest's byte order",
     ADAPTER_ADDRINFO,
     """            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let mut address = [0u8; 4];""",
     """            let port = u16::from_le_bytes([bytes[2], bytes[3]]);
            let mut address = [0u8; 4];""",
     ANDROID),

    # Rule 1 in person: an unclassified host failure given a specific, actionable errno.
    ("sock-A10", "A", "an unclassified network failure acquires a plausible errno",
     ADAPTER_NET,
     """        // `NetErrorKind::Other` and nothing else. Spelled as a wildcard because the enum is
        // `#[non_exhaustive]`, and a kind added upstream without a decision here must refuse by
        // name rather than acquire a plausible errno.
        _ => return None,""",
     """        _ => consts::EIO,""",
     ANDROID),

    # The over-corrections. Each reads as *more* careful than the code it replaces, and each
    # breaks something a real caller does.
    ("sock-B1", "B", "freeaddrinfo(NULL) refuses by name instead of doing nothing",
     ADAPTER_NET,
     """    let res = c.args().next_u64()?;
    if res == 0 {
        return Ok(());
    }""",
     """    let res = c.args().next_u64()?;""",
     ANDROID),

    # Mutating the CONSTANT would not compile -- there is a `const _: () = assert!` beside it --
    # and the harness files a mutation that does not compile as a MISS. So the row bounds the
    # function instead, which is where a real over-correction would land anyway.
    ("sock-B2", "B", "a socket transfer is capped at a page, below one TLS record",
     ADAPTER_NET,
     """    usize::try_from(count).unwrap_or(usize::MAX).min(SOCKET_IO_BLOCK)""",
     """    usize::try_from(count).unwrap_or(usize::MAX).min(omni_platform::fs::IO_BLOCK)""",
     ANDROID),

    # ---- M6: the socket configuration the client-settings fetch actually walks -----------------
    #
    # Every row here has the same failure mode and it is why the prefix exists: **a wrong socket
    # option number is accepted by the host**. There is no error, no log line and no test that
    # merely calls the function can see it -- which is exactly the shape `VERIFICATION.md` rule 1
    # is about, one layer lower than a stub.

    ("sockcfg-A1", "A", "the guest's TCP_KEEPIDLE is routed to the probe INTERVAL instead of the idle time",
     ADAPTER_NET,
     """            (IPPROTO_TCP, TCP_KEEPIDLE) => int_option(4)?.and_then(|seconds| {
                positive_seconds(seconds).map(SocketOption::KeepAliveIdle)
            }),""",
     """            (IPPROTO_TCP, TCP_KEEPIDLE) => int_option(4)?.and_then(|seconds| {
                positive_seconds(seconds).map(SocketOption::KeepAliveInterval)
            }),""",
     ANDROID),

    # The guest-side constant given the HOST's number. It compiles, it is a real Windows option,
    # and the only thing that can tell is a test that asserts the Linux value or one that round
    # trips three distinct values.
    ("sockcfg-A2", "A", "TCP_KEEPINTVL carries Windows' number (17) instead of Linux's (5)",
     ADAPTER_NET,
     """const TCP_KEEPINTVL: i32 = 5;""",
     """const TCP_KEEPINTVL: i32 = 17;""",
     ANDROID),

    # **MEASURED NOT CAUGHT, and kept as a miss rather than retargeted.** Zero survives every
    # conversion between the guest and the host and means "probe with no idle time" when it
    # arrives -- but Winsock refuses a zero keep-alive figure ITSELF, with an error this seam maps
    # to EINVAL, so the guest sees 22 with or without the check and no test that runs on this host
    # can separate them. Linux range-checks all three the same way. The check is for a stack that
    # would accept zero, there is no such stack among the five targets, and `positive_seconds`'s
    # documentation says so in as many words. Leaving the row here is the point: a row that cannot
    # be caught on the tested host is a statement about the host, and deleting it would delete the
    # statement.
    ("sockcfg-A3", "A", "a zero keep-alive figure is accepted instead of EINVAL",
     ADAPTER_NET,
     """    u32::try_from(value).ok().filter(|seconds| *seconds > 0).map(|seconds| Duration::from_secs(u64::from(seconds)))""",
     """    u32::try_from(value).ok().map(|seconds| Duration::from_secs(u64::from(seconds)))""",
     ANDROID),

    # `getsockname` shares `write_peer` with `recvfrom`, so one wrong answer here is two. Reporting
    # what FITTED rather than the full length tells a caller its buffer was big enough.
    ("sockcfg-A4", "A", "a truncated sockaddr reports the length that fitted, not the real one",
     ADAPTER_NET,
     """    view.mem().write_u32(length_to, len as u32, blame)""",
     """    view.mem().write_u32(length_to, copied as u32, blame)""",
     ANDROID),

    # The read half, in the adapter: an `int` of seconds written as a 16-byte `struct timeval`.
    ("sockcfg-A5", "A", "a keep-alive interval is written back as a struct timeval",
     ADAPTER_NET,
     """            OptionValue::Interval(interval) => {
                (interval.as_secs().min(i32::MAX as u64) as i32).to_le_bytes().to_vec()
            }""",
     """            OptionValue::Interval(interval) => timeval_bytes(Some(interval)).to_vec(),""",
     ANDROID),

    # **The host side of the same defect**, in the only file that knows Windows' numbers. The two
    # that are transposed here are the ones a reader is most likely to assume are the same option
    # in both schemes, because they share a NAME.
    ("sockcfg-A6", "A", "the host's TCP_KEEPINTVL and TCP_KEEPCNT are transposed",
     PLATFORM_NET_WINDOWS,
     """        IPPROTO_TCP,
        TCP_KEEPINTVL,
        option_i32(seconds, "TCP_KEEPINTVL (the keep-alive probe interval)")?,""",
     """        IPPROTO_TCP,
        TCP_KEEPCNT,
        option_i32(seconds, "TCP_KEEPINTVL (the keep-alive probe interval)")?,""",
     PLATFORM),

    # A fraction of a second rounded rather than refused: on a host that counts these in whole
    # seconds, 500 ms becomes 0 -- probe immediately -- and `setsockopt` answers 0.
    ("sockcfg-B1", "B", "a fractional keep-alive figure is rounded down instead of refused",
     PLATFORM_NET_MOD,
     """    if value.subsec_nanos() != 0 {""",
     """    if false {""",
     PLATFORM),

    # The predicate the wait cap turns on, which is the other half of this session's work. A
    # `can_change` that answers false puts the 60-second bound back on a `poll` over a socket, and
    # that refuses the 69.001s wait the engine's HTTP stack asks for over the settings socket --
    # killing the thread carrying the connection. Anchored on the predicate rather than on
    # `bounded_wait`'s condition because the predicate has a unit test and the condition needs an
    # `ImportCall`, which is the same reasoning `clocks::capped` is a function for.
    ("sockcfg-B2", "B", "a set naming a socket reports that nothing in it can become ready",
     ADAPTER_NET,
     """    fn can_change(&self) -> bool {
        !self.sockets.is_empty() || self.gate
    }""",
     """    fn can_change(&self) -> bool {
        self.gate
    }""",
     ANDROID_LIB),

    # ---- the socket record: off by default, and bounded ----------------------------------------
    #
    # **A recorder that captures while it is switched off is the defect this module exists to not
    # have.** It would pass every other test -- the bytes are right, the counts are right, the
    # outline is right -- and it would be quietly accumulating the guest's cookies in a process
    # global in every run of the whole suite. Nothing but an assertion about the *off* state can
    # see it.
    ("netrec-A1", "A", "the record keeps capturing after it is switched off",
     PLATFORM_NET_RECORD,
     """    if limit == 0 || bytes.is_empty() {""",
     """    if bytes.is_empty() {""",
     PLATFORM),

    # The budget applied to the call instead of to the buffer: every `send` then appends up to
    # `limit` more bytes, so a long connection grows without bound and the transcript stops being a
    # prefix. It reads as correct, and a short run cannot tell the two apart.
    ("netrec-A2", "A", "the byte budget bounds each call rather than the whole capture",
     PLATFORM_NET_RECORD,
     """    let room = limit.saturating_sub(buffer.len());""",
     """    let room = limit;""",
     PLATFORM),

    # **The one thing in a TLS session that is in the clear, read off by one.** The server name
    # would come back with the extension's length byte in front of it -- still recognisable to a
    # human, still "containing" the hostname, and naming a host that was never contacted.
    ("netrec-B1", "B", "the ClientHello server name is taken one byte early",
     PLATFORM_NET_RECORD,
     """            let host = data.get(5..)?;""",
     """            let host = data.get(4..)?;""",
     PLATFORM),

    # ---- the application name the client-settings request is built from --------------------------
    #
    # **The defect this is the regression test for, put back exactly.** `"android"` is well-formed,
    # plausible, and in the APK's dex files as a string -- and it made the engine ask
    # `clientsettingscdn.roblox.com` for an application that does not exist, which answers
    # `HTTP 400 {"errors":[{"code":1,"message":"The application name is invalid."}]}`. Every test in
    # the suite passed with it in place for the whole of M6. What catches it is the one assertion
    # that is about the APK rather than about plausibility: that the string is what `bh.x0.M`
    # returns.
    ("appname-A1", "A", "the application name goes back to the invented `android`",
     JNI_SCRIPT,
     """pub const CHANNEL_PLATFORM_NAME: &str = "GoogleAndroidApp";""",
     """pub const CHANNEL_PLATFORM_NAME: &str = "android";""",
     GATE_APPNAME),

    # The engine's own compiled-in fallback, which is a *real* application name and answers
    # `HTTP 200` -- so no run and no network would tell it from the right one. It is still wrong:
    # it says this is a build with no Java side rather than the Google Play build the APK is, and
    # the two get different flag documents (1,322,077 bytes against 1,351,777, MEASURED).
    ("appname-B1", "B", "the application name is the binary's fallback instead of the APK's",
     JNI_SCRIPT,
     """pub const CHANNEL_PLATFORM_NAME: &str = "GoogleAndroidApp";""",
     """pub const CHANNEL_PLATFORM_NAME: &str = "AndroidApp";""",
     GATE_APPNAME),

    # ---- §8 row 26: touch input, `vk.e.onTouch` -> `nativePassInput` ------------------------------
    #
    # **Pixels where the engine reads density-independent pixels.** `vk.e` divides by
    # `DisplayMetrics.density` (`0x01fe`), and on this host at 100% scaling the density is 1.0 --
    # so the gate cannot see this, and a test written at 1.0 cannot either. The detectors are at
    # 1.5.
    ("input-A1", "A", "a press reaches the engine in pixels instead of dp",
     JNI_INPUT,
     """                pointer.set_x(x / scale);
                pointer.set_y(y / scale);
                pointer.set_state(STATE_BEGAN);""",
     """                pointer.set_x(x);
                pointer.set_y(y);
                pointer.set_state(STATE_BEGAN);""",
     INPUT),

    # **The two floats in each other's registers.** x is `s0` and y is `s1` (`0x02bbbab0`,
    # `0x02bbbaa8`); swapped, every touch lands mirrored in the diagonal and nothing fails.
    ("input-A2", "A", "x and y are passed in each other's registers",
     JNI_INPUT,
     """        GuestArg::Float(call.x),
        GuestArg::Float(call.y),""",
     """        GuestArg::Float(call.y),
        GuestArg::Float(call.x),""",
     INPUT),

    # **The pointer id where the state goes.** Both are small integers, and for a first press both
    # are zero -- which is why the detector's drag and release are what see it.
    ("input-A3", "A", "the pointer id and the state trade registers",
     JNI_INPUT,
     """        int(call.pointer_id),
        GuestArg::Float(call.x),
        GuestArg::Float(call.y),
        int(call.state),""",
     """        int(call.state),
        GuestArg::Float(call.x),
        GuestArg::Float(call.y),
        int(call.pointer_id),""",
     INPUT),

    # **`D.b()` ignored**: touches sent before the engine has a surface, which the Java side never
    # does (`0x02b3`).
    ("input-A4", "A", "touches are sent while the surface is dead",
     JNI_INPUT,
     """            if ready {""",
     """            if true {""",
     INPUT),

    # **An ended pointer is kept.** `0x02f6` forgets it whether or not it was sent, so a release
    # while the surface is dead must not come back as a move once it is alive.
    ("input-A5", "A", "an ended pointer is not forgotten",
     JNI_INPUT,
     """            self.pointers.remove(&id);""",
     """            let _ = id;""",
     INPUT),

    # **The dedup dropped**: a move that does not move is sent anyway. Harmless-looking, and it is
    # a `nativePassInput` per frame per finger held still.
    ("input-A6", "A", "a move that does not move is sent",
     JNI_INPUT,
     """            } else if pointer.state == pointer.prev_state {
                changed""",
     """            } else if pointer.state == pointer.prev_state {
                true""",
     INPUT),

    # **The view in pixels.** `vk.e` divides the view's size too (`0x02c0`). The native does not
    # read it in this build (`w4`/`w5` are never read), which is exactly why nothing but a test
    # would notice.
    ("input-A7", "A", "the view's size is sent in pixels",
     JNI_INPUT,
     """        let width = (view.0 as f32 / scale) as i32;""",
     """        let width = view.0 as i32;""",
     INPUT),

    # **The surface starts alive.** `jk.o0.a` is a boolean field, false until `surfaceCreated`.
    ("input-A8", "A", "the seam delivers before it is told the surface exists",
     JNI_INPUT,
     """            surface_alive: false,""",
     """            surface_alive: true,""",
     INPUT),

    # **A finger that never lifts.** A capture something else takes ends with no button-up, and
    # the engine's thumbstick would be held for ever.
    ("input-A9", "A", "losing focus mid-press does not cancel the touch",
     JNI_INPUT,
     """                Some((x, y)) => vec![touch(Action::Cancel, x, y)],""",
     """                Some(_) => Vec::new(),""",
     INPUT),

    # **The neighbouring export.** `nativePassInputBatch` exists, is exported, and takes a
    # different argument list; resolving it would call the engine with the wrong shape.
    ("input-A10", "A", "the seam resolves the batch native instead",
     JNI_INPUT,
     """pub const PASS_INPUT_SYMBOL: &str = "Java_com_roblox_engine_jni_NativeInputInterface_nativePassInput";""",
     """pub const PASS_INPUT_SYMBOL: &str = "Java_com_roblox_engine_jni_NativeInputInterface_nativePassInputBatch";""",
     INPUT),

    # **A zero density admitted**: every coordinate becomes infinity -- MEASURED what a zero did one
    # field over, in the renderer.
    ("input-A11", "A", "a density of zero is accepted",
     JNI_INPUT,
     """        if !(density.is_finite() && density > 0.0) {""",
     """        if false {""",
     INPUT),

    # **A release somewhere else, without the move there.** `vk.e` ignores an up's own position,
    # so the engine would see the finger lift where it last was rather than where it lifted.
    ("input-A12", "A", "a release away from the last move is not preceded by a move",
     JNI_INPUT,
     """vec![touch(Action::Move, x, y), touch(Action::Up, x, y)]""",
     """vec![touch(Action::Up, x, y)]""",
     INPUT),

    # **Over-correction: the up takes its own position.** It reads as more accurate and is not
    # what `0x01bc` does -- the Java side reports the pointer where it last moved to.
    ("input-B1", "B", "an up reports the up event's own position",
     JNI_INPUT,
     """                if let Some(pointer) = self.pointers.get_mut(&event.pointer) {
                    pointer.set_state(STATE_ENDED);
                }""",
     """                if let Some(pointer) = self.pointers.get_mut(&event.pointer) {
                    if let Some((x, y)) = event.position(event.pointer) {
                        pointer.set_x(x / scale);
                        pointer.set_y(y / scale);
                    }
                    pointer.set_state(STATE_ENDED);
                }""",
     INPUT),

    # **Over-correction: every press is sent.** The Java side's dedup reaches a press at `(0, 0)`,
    # because a fresh `vk.e$h` is all zeros; "fixing" that is a different listener.
    ("input-B2", "B", "a press is always sent, even one that changes nothing",
     JNI_INPUT,
     """            } else if pointer.state == pointer.prev_state {
                changed""",
     """            } else if pointer.state == pointer.prev_state {
                changed || pointer.state == STATE_BEGAN""",
     INPUT),

    # **Over-correction: a hover is a touch.** A touchscreen reports no hover, and a mouse moving
    # over the window with no button held is not a finger.
    ("input-B3", "B", "a hover becomes a move",
     JNI_INPUT,
     """                    vec![touch(Action::Move, x, y)]
                }
                None => Vec::new(),""",
     """                    vec![touch(Action::Move, x, y)]
                }
                None => vec![touch(Action::Move, x, y)],""",
     INPUT),

    # **Over-correction: every button is a finger.** A right-click would then press whatever is
    # under the pointer.
    ("input-B4", "B", "any pointer button puts the finger down",
     JNI_INPUT,
     """            WindowEvent::PointerDown { button: PointerButton::Primary, x, y } => {""",
     """            WindowEvent::PointerDown { x, y, .. } => {""",
     INPUT),

    # **Over-correction: the view rounded.** Java's `float-to-int` truncates (`0x02c1`).
    ("input-B5", "B", "the view's size is rounded to the nearest dp instead of truncated",
     JNI_INPUT,
     """        let width = (view.0 as f32 / scale) as i32;""",
     """        let width = (view.0 as f32 / scale).round() as i32;""",
     INPUT),

    # ---- hardware keys, `vk.g` -> `nativePassKeyEvent` ---------------------------------------------
    #
    # **Every key one to the right.** The engine turns the scan code into a key through its own
    # table (`0x6e6414`), so an off-by-one here is W typing E -- and no call fails.
    ("keys-A1", "A", "the host make code is off by one from the Linux input code",
     JNI_KEYS,
     """            0x01..=0x53 | 0x56..=0x58 => Some(make as u16),""",
     """            0x01..=0x53 | 0x56..=0x58 => Some(make as u16 + 1),""",
     INPUT),

    # **The extended flag ignored**: the Up arrow becomes keypad 8, right Ctrl becomes left.
    ("keys-A2", "A", "an extended key is read as its unextended twin",
     JNI_KEYS,
     """    match scancode & !0xFF {""",
     """    match scancode & !0xE0FF {""",
     INPUT),

    # **Pause read as Num Lock**: they share make code 0x45, and only the flag parts them.
    ("keys-A3", "A", "Pause is sent as Num Lock",
     JNI_KEYS,
     """            0x45 => Some(119),""",
     """            0x45 => Some(69),""",
     INPUT),

    # **The scan code in the key code's register.** `w4` is never read (`0x02baebdc`), so the engine
    # would receive the Android key code as a scan code and ignore the real one.
    ("keys-A4", "A", "the scan code and the key code trade registers",
     JNI_KEYS,
     """        int(call.scan_code),
        int(call.key_code),""",
     """        int(call.key_code),
        int(call.scan_code),""",
     INPUT),

    # **Auto-repeat dropped.** `getRepeatCount() > 0` is the fourth argument; the engine reads it.
    ("keys-A5", "A", "an auto-repeat is sent as a fresh press",
     JNI_KEYS,
     """        WindowEvent::KeyDown { scancode, repeat, .. } => (true, scancode, repeat),""",
     """        WindowEvent::KeyDown { scancode, .. } => (true, scancode, false),""",
     INPUT),

    # **A release sent as a press**: every key held for ever.
    ("keys-A6", "A", "a key-up is sent as a key-down",
     JNI_KEYS,
     """        WindowEvent::KeyUp { scancode, .. } => (false, scancode, false),""",
     """        WindowEvent::KeyUp { scancode, .. } => (true, scancode, false),""",
     INPUT),

    # **No keyboard declared, keys sent anyway**: a Java side the engine's own `Configuration`
    # contradicts.
    ("keys-A7", "A", "the seam is built without a declared hardware keyboard",
     JNI_KEYS,
     """        if declared
            != (""",
     """        if false && declared
            != (""",
     INPUT),

    # **BACK and the volume keys passed.** `vk.g.a` withholds them (`0x0006`-`0x000f`).
    ("keys-A8", "A", "vk.g.a's withheld keys are passed",
     JNI_KEYS,
     """    !WITHHELD_KEY_CODES.contains(&key_code)""",
     """    true""",
     INPUT),

    # **The window seam drops the extended flag**, one layer down from keys-A2: right Ctrl and left
    # Ctrl arrive as the same key.
    ("keys-A9", "A", "the window seam's scan code loses the extended flag",
     PLAT_WINDOW_WINDOWS,
     """    if (lparam >> 24) & 1 != 0 { 0xE000 | make } else { make }""",
     """    make""",
     PLATFORM_LIB),

    # **Over-correction: every make code is its own input code.** 84 is no key, and 85's set-1 code
    # is not 0x55; past the 88 the two numberings part.
    ("keys-B1", "B", "every make code up to 0x7f is passed through as an input code",
     JNI_KEYS,
     """            0x01..=0x53 | 0x56..=0x58 => Some(make as u16),""",
     """            0x01..=0x7F => Some(make as u16),""",
     INPUT),

    # **Over-correction: an unknown extended key falls back to its make code** -- a key the engine
    # would receive as some other key.
    ("keys-B2", "B", "an extended key outside the table falls back to its make code",
     JNI_KEYS,
     """        0xE000 => EXTENDED.iter().find(|(code, _)| *code == make).map(|&(_, evdev)| evdev),""",
     """        0xE000 => EXTENDED
            .iter()
            .find(|(code, _)| *code == make)
            .map(|&(_, evdev)| evdev)
            .or(Some(make as u16)),""",
     INPUT),

    # **Over-correction: the whole upper half of LPARAM as the code** -- the key-up transition bits
    # and the flag folded into the make code.
    ("keys-B3", "B", "the window seam's make code takes more than bits 16-23",
     PLAT_WINDOW_WINDOWS,
     """    let make = ((lparam >> 16) & 0xff) as u32;""",
     """    let make = ((lparam >> 16) & 0xffff) as u32;""",
     PLATFORM_LIB),

    # ---- `/proc/meminfo` and `/proc/self/statm` ------------------------------------------------
    # MEASURED: every gate run logged `Failed to open` for both, many times a second. The engine
    # opens each once, keeps the descriptor and `pread`s it at 0 for every reading (`libroblox.so`
    # `0x2282674`, `0x228296c`). meminfo is the embedding's budget -- `sysinfo`'s own numbers --
    # and statm is this process as the host measures it; `bionic/procfs.rs` has the table.
    ("procfs-A1", "A", "the two /proc files are not served, and the engine is told ENOENT again",
     ADAPTER_MOD,
     """        procfs::serve(&filesystem, Arc::clone(&self.memory_budget), self.space.page_size())""",
     """        Ok::<(), omni_platform::fs::FsError>(())""",
     PROCFS),

    # The engine keeps the descriptor for the life of the run: a file generated once would give it
    # one memory reading for ever.
    ("procfs-A2", "A", "a read from offset 0 on a kept descriptor does not generate a new reading",
     PLAT_FS,
     """        if offset == 0 || self.snapshot.is_none() {""",
     """        if self.snapshot.is_none() {""",
     PLATFORM_LIB),

    ("procfs-A3", "A", "MemFree ignores the commit charge and reports the whole budget free",
     ADAPTER_PROCFS,
     """        let free = kb(total.saturating_sub(charged));""",
     """        let free = kb(total);""",
     PROCFS),

    # The number this project budgets against, in the field named for residency -- which D10
    # measured differing from it by 1019 MB for one untouched gigabyte.
    ("procfs-A4", "A", "statm's resident is the commit charge rather than the working set",
     ADAPTER_PROCFS,
     """            resident: pages(memory.resident),""",
     """            resident: pages(memory.commit_charge),""",
     PROCFS),

    ("procfs-A5", "A", "statm's shared is the whole working set rather than its shareable part",
     PLAT_VM_WINDOWS,
     """        resident_shared: resident.saturating_sub(counters.PrivateWorkingSetSize as u64),""",
     """        resident_shared: resident,""",
     PROCESS_MEMORY),

    ("procfs-A6", "A", "statm's size is the whole user address space rather than what is in use",
     PLAT_VM_WINDOWS,
     """    Ok(status.ullTotalVirtual.saturating_sub(status.ullAvailVirtual))""",
     """    Ok(status.ullTotalVirtual)""",
     PROCESS_MEMORY),

    ("procfs-A7", "A", "meminfo loses Linux's 16-and-8 column layout",
     ADAPTER_PROCFS,
     """            let _ = writeln!(text, "{label:<16}{kb:>8} kB");""",
     """            let _ = writeln!(text, "{label} {kb} kB");""",
     PROCFS),

    # A file that cannot be produced must be refused by the call that asked, naming why -- here,
    # the missing budget -- not handed out as a descriptor whose reads then fail.
    ("procfs-A8", "A", "a generated file that cannot be produced opens anyway, empty",
     PLAT_FS,
     """        let first = generate()?;""",
     """        let first = generate().unwrap_or_default();""",
     PLATFORM_LIB),

    # **Over-correction: "true of this host"** read as the host's memory. `sysinfo` and
    # `_SC_PHYS_PAGES` answer the embedding's budget, and this file must say what they say.
    ("procfs-B1", "B", "MemTotal is a host-sized figure rather than the embedding's budget",
     ADAPTER_PROCFS,
     """    Ok(Meminfo::of(total, charged, page).render().into_bytes())""",
     """    Ok(Meminfo::of(16 << 30, charged, page).render().into_bytes())""",
     PROCFS),

    # **Over-correction: always fresh.** Generating on every read splices two readings into one
    # when the file is taken in pieces -- and the adapter's own `pread` takes it in pieces.
    ("procfs-B2", "B", "every read generates afresh, so a reading in pieces is several readings",
     PLAT_FS,
     """        if offset == 0 || self.snapshot.is_none() {""",
     """        if true {""",
     PLATFORM_LIB),

    # **Over-correction: permissive.** A /proc file is 0444 and an app is told EACCES for writing.
    ("procfs-B3", "B", "a generated file can be opened for writing",
     PLAT_FS,
     """        if flags.write || flags.truncate {""",
     """        if false {""",
     PLATFORM_LIB),

    # **Over-correction: by prefix.** Every /proc path answered by one generator, where a path
    # nothing serves must stay ENOENT and be recorded as a miss.
    ("procfs-B4", "B", "every /proc path is served by a generated file",
     PLAT_FS,
     """        let generate = Arc::clone(generated.0.get(&name)?);""",
     """        let generate = Arc::clone(generated.0.get(&name).or_else(|| {
            name.starts_with("/proc/").then(|| generated.0.values().next()).flatten()
        })?);""",
     PLATFORM_LIB),

    ("procfs-B5", "B", "MemAvailable reports the whole budget as reclaimable",
     ADAPTER_PROCFS,
     """            mem_available: free,""",
     """            mem_available: kb(total),""",
     PROCFS),

    # **Over-correction: the whole image as code.** Linux's `text` is the executable's `PF_X`
    # span; the image's data is not code.
    ("procfs-B6", "B", "statm's text spans the whole executable image, data included",
     PLAT_VM_WINDOWS,
     """        if characteristics & IMAGE_SCN_MEM_EXECUTE == 0 {""",
     """        if false && characteristics & IMAGE_SCN_MEM_EXECUTE == 0 {""",
     PROCESS_MEMORY),

    # ---- this session: input, text, shutdown, second launch ------------------------------------
    ("semintr-A1", "A", "sem_wait keeps looping in the host on a futex the shutdown interrupted",
     "crates/omni-bionic/src/sem.rs",
     """        if futex.interrupted() {""",
     """        if false && futex.interrupted() {""",
     BIONIC),
    ("mutexintr-A1", "A", "a contended lock keeps looping in the host on an interrupted futex",
     "crates/omni-bionic/src/mutex.rs",
     """        if futex.interrupted() {""",
     """        if false && futex.interrupted() {""",
     BIONIC),
    ("clearerr-A1", "A", "clearerr leaves the error indicator set",
     "crates/omni-bionic/src/stdio.rs",
     """    stream.eof = false;
    stream.error = false;
}""",
     """    stream.eof = false;
}""",
     BIONIC),
    ("utf8len-A1", "A", "GetStringUTFLength answers the UTF-16 length",
     JNI_ENV,
     """            Ok(JniReturn::Int(i32::try_from(text.modified_utf8_len()).unwrap_or(i32::MAX)))""",
     """            Ok(JniReturn::Int(i32::try_from(text.len()).unwrap_or(i32::MAX)))""",
     ANDROID_LIB),
    ("bytearray-A1", "A", "NewByteArray is unbound again",
     JNI_ENV,
     """        "NewByteArray" => {""",
     """        "NewByteArrayX" => {""",
     ANDROID_LIB),
    ("bytearray-A2", "A", "SetByteArrayRegion writes from index 0 instead of start",
     JNI_ENV,
     """                values[start as usize + index] = *byte as i8;""",
     """                values[index] = *byte as i8;""",
     ANDROID_LIB),
    ("construct-A1", "A", "NativeTextBoxInfo's constructor drops its arguments again",
     JNI_CLASSES,
     """        methods: &[m("<init>", "(FFFFFZIIIIIIZZZ)V", Answer::Construct(TEXT_BOX_INFO))],""",
     """        methods: &[m("<init>", "(FFFFFZIIIIIIZZZ)V", Answer::NewInstance)],""",
     ANDROID_LIB),
    ("kbraw-B1", "B", "manualFocusRelease is read even when the call does not lay the field out",
     JNI_ENV,
     """                (true, info) if info != 0 => {""",
     """                (_, info) if info != 0 => {""",
     ANDROID_LIB),
    ("textmodel-A1", "A", "Backspace deletes half a surrogate pair",
     "crates/omni-android/src/jni/text.rs",
     """        if start > 0 && low(self.text[start]) && high(self.text[start - 1]) {""",
     """        if false {""",
     ANDROID_LIB),
    ("textmodel-B1", "B", "Enter closes the field even when the box asked for manual focus release",
     "crates/omni-android/src/jni/text.rs",
     """        if !self.manual_focus_release {""",
     """        if true {""",
     ANDROID_LIB),
    ("buildfields-A1", "A", "Build declares MANUFACTURER only, so BOARD's lookup answers 0 again",
     JNI_CLASSES,
     """        fields: BUILD_FIELDS,""",
     """        fields: &[sf("MANUFACTURER", "Ljava/lang/String;", Answer::Unanswered)],""",
     ANDROID_LIB),
    ("utime-A1", "A", "utime swaps the utimbuf's access and modification times",
     "crates/omni-android/src/bionic/files.rs",
     """            (when(seconds(0)), when(seconds(8)))""",
     """            (when(seconds(8)), when(seconds(0)))""",
     ANDROID),
    ("atof-A1", "A", "atof truncates to an integer",
     "crates/omni-android/src/bionic/handlers.rs",
     """    fn atof(s: ptr) -> f64 = |v| omni_bionic::numerics::atof(&mut v, s);""",
     """    fn atof(s: ptr) -> f64 = |v| omni_bionic::numerics::atof(&mut v, s).map(f64::trunc);""",
     ANDROID),
    ("wmchar-A1", "A", "a control code from WM_CHAR is passed on as typed text",
     "crates/omni-platform/src/window/windows.rs",
     """    let control = character < ' ' || character == '\\u{7F}';""",
     """    let control = false;""",
     PLATFORM_LIB),
    ("shutdownlock-A1", "A", "a lock wait the shutdown interrupts returns EINTR to the guest",
     "crates/omni-android/src/bionic/handlers.rs",
     """    if code == omni_bionic::errno::consts::EINTR && omni_bionic::threads::Futex::interrupted(futex) {""",
     """    if false {""",
     ANDROID),

    # How earlier runs ended, as the engine is handed them. The engine matches the reason TEXT
    # (`jk.l2.b` cuts it out of `toString()`), so the constant's Java name is the tempting wrong one.
    ("exits-A1", "A", "the reason is spelled as the constant, not as reasonCodeToString spells it",
     JNI_MOD,
     """            Self::REASON_USER_REQUESTED => Ok("USER REQUESTED"),""",
     """            Self::REASON_USER_REQUESTED => Ok("USER_REQUESTED"),""",
     ANDROID_LIB),
    # The answer the list had before it had elements: every list empty again.
    ("exits-A2", "A", "List.size() answers 0 whatever the receiver holds",
     JNI_CLASSES,
     """            m("size", "()I", Answer::ListSize),""",
     """            m("size", "()I", Answer::Int(0)),""",
     ANDROID_LIB),
    # A null where Java throws: the engine would read a record that is not there.
    ("exits-A3", "A", "List.get() past the end answers null instead of refusing",
     JNI_ENV,
     """                    match usize::try_from(index).ok().and_then(|at| elements.get(at)) {""",
     """                    match usize::try_from(index).ok().and_then(|at| elements.get(at)).or(Some(&None)) {""",
     ANDROID_LIB),
    # Oldest first: the bound would then drop the newest run, the one the next launch asks about.
    ("exits-A4", "A", "the gate's exit records are kept oldest first",
     GATE_ACTIVITY_FILE,
     """    let mut records = vec![exit];
    records.extend(read_exit_records(root));""",
     """    let mut records = read_exit_records(root);
    records.push(exit);""",
     GATE_EXITS),
    # The plausible stub: an event nothing can deliver reported as delivered. The engine then never
    # hears the app went to the background, and says so only at the next launch, as a crash.
    ("lifecycle-A1", "A", "a process event whose export nothing resolves is reported as sent",
     JNI_SCRIPT,
     """    let Some(target) = resolve(&symbol) else {
        return Err(AbiError::JniRefused {
            function: symbol,""",
     """    let Some(target) = resolve(&symbol) else {
        return Ok(()); #[allow(unreachable_code)]
        return Err(AbiError::JniRefused {
            function: symbol,""",
     ANDROID_LIB),

    # libaaudio.so. The first is the one the whole module exists for: a started stream whose thread
    # never calls the guest's callback still "runs", and nothing but the samples says otherwise.
    ("aaudio-A1", "A", "the data-callback thread never calls the guest's callback",
     "crates/omni-android/src/aaudio/mod.rs",
     """        while allowed >= burst_frames {""",
     """        while allowed >= burst_frames * 1000 {""",
     AAUDIO),
    ("aaudio-A2", "A", "dlopen does not supply libaaudio.so -- FMOD's NOSOUND fallback again",
     "crates/omni-android/src/bionic/dl.rs",
     """    (crate::aaudio::SONAMES[0], crate::aaudio::ENTRY_POINT),""",
     """    ("libnothing.so", crate::aaudio::ENTRY_POINT),""",
     AAUDIO),
    ("aaudio-A3", "A", "an AAudio name the library does not export is a NULL, not a refusal",
     "crates/omni-android/src/bionic/dl.rs",
     """        if audio && !exported && wanted.starts_with("AAudio") {""",
     """        if false {""",
     AAUDIO),
    ("aaudio-A4", "A", "an input stream opens instead of answering UNAVAILABLE",
     "crates/omni-android/src/aaudio/mod.rs",
     """    if builder.direction == consts::DIRECTION_INPUT {""",
     """    if false {""",
     AAUDIO),
    ("aaudio-A5", "A", "an unspecified format becomes int16 rather than the host mix format's float",
     "crates/omni-android/src/aaudio/mod.rs",
     """        consts::FORMAT_UNSPECIFIED | consts::FORMAT_PCM_FLOAT => Some(consts::FORMAT_PCM_FLOAT),
        consts::FORMAT_PCM_I16 => Some(consts::FORMAT_PCM_I16),""",
     """        consts::FORMAT_PCM_FLOAT => Some(consts::FORMAT_PCM_FLOAT),
        consts::FORMAT_UNSPECIFIED | consts::FORMAT_PCM_I16 => Some(consts::FORMAT_PCM_I16),""",
     AAUDIO),
    ("aaudio-A6", "A", "int16 samples are scaled by 32767, so full scale is not -1.0",
     "crates/omni-android/src/aaudio/mod.rs",
     """            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)""",
     """            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32767.0)""",
     AAUDIO),

    # Writable MAP_SHARED file mappings: the engine's `MappedFile` (libroblox.so link 0x2273210),
    # which three threads of the first signed-in session died asking for. The mapping is a host
    # view of a PAGE_READWRITE section over the guest's own descriptor, so "the write-back" is the
    # view being shared at all: A2-A4 break that at the seam, A1 reverts the fix, A10 keeps the
    # section alive past the munmap (the engine re-opens the file O_TRUNC right after). The file on
    # disk is the detector throughout. MS_SYNC's own flush is NOT a row: the view is coherent with
    # every reader without it (measured), so nothing a test can read distinguishes a skipped flush
    # -- only durability across a power loss would, and the rows below pin what `sync` selects.
    ("sharedmap-A1", "A", "a writable MAP_SHARED file mapping is refused again",
     ADAPTER_GUESTMEM,
     """        file_backed && prot == PROT_READ | PROT_WRITE && flags & MAP_TYPE == MAP_SHARED;""",
     """        false && file_backed && prot == PROT_READ | PROT_WRITE && flags & MAP_TYPE == MAP_SHARED;""",
     PROCFS),
    ("sharedmap-A2", "A", "a shared file view is created copy-on-write, so stores never reach the file",
     PLAT_VM_WINDOWS,
     """        (PAGE_READWRITE, (size as u64).min(in_file) as usize)""",
     """        (PAGE_WRITECOPY, (size as u64).min(in_file) as usize)""",
     PROCFS),
    ("sharedmap-A3", "A",
     "a shared view is created with the protection asked for, so a later raise is copy-on-write",
     PLAT_VM_WINDOWS,
     """        (PAGE_READWRITE, (size as u64).min(in_file) as usize)""",
     """        (if protection == Protection::ReadWrite { PAGE_READWRITE } else { view_protection(protection) }, (size as u64).min(in_file) as usize)""",
     MEM),
    ("sharedmap-A4", "A", "a shared view is not clipped to the file, so a partial last page fails",
     PLAT_VM_WINDOWS,
     """        (PAGE_READWRITE, (size as u64).min(in_file) as usize)""",
     """        (PAGE_READWRITE, size)""",
     PROCFS),
    ("sharedmap-A5", "A", "the seam refuses a shared view ending inside the file's last page",
     PLAT,
     """    let limit = if file.is_shared() {""",
     """    let limit = if false {""",
     MEM),
    ("sharedmap-A6", "A", "an O_RDONLY descriptor is shared anyway: a host refusal, not EACCES",
     PLAT_FS,
     """            Some(Entry::File { writable: false, guest, .. }) => Err(FsError::kinded(""",
     """            Some(Entry::File { writable: false, guest, .. }) if false => Err(FsError::kinded(""",
     PROCFS),
    ("sharedmap-A7", "A", "msync never reports a hole inside the space as ENOMEM",
     ADAPTER_GUESTMEM,
     """            None => hole = true,""",
     """            None => break,""",
     PROCFS),
    ("sharedmap-A8", "A", "GuestSpace::sync selects no view at all",
     SPACE,
     """                    if !matches!(entry.os, OsState::View { .. }) || !backing.is_shared() {""",
     """                    if true {""",
     MEM),
    ("sharedmap-A10", "A", "the section outlives the munmap, so the engine's O_TRUNC re-open fails",
     ADAPTER_GUESTMEM,
     """    match space.map_file(&backing, offset, placement, len, Protection::ReadWrite) {""",
     """    std::mem::forget(std::sync::Arc::clone(&backing));
    match space.map_file(&backing, offset, placement, len, Protection::ReadWrite) {""",
     PROCFS),
    ("sharedmap-B1", "B", "GuestSpace::sync writes back private file views too",
     SPACE,
     """                    if !matches!(entry.os, OsState::View { .. }) || !backing.is_shared() {""",
     """                    if !matches!(entry.os, OsState::View { .. }) {""",
     MEM),
    ("sharedmap-B2", "B", "a mapping ending inside the file's last page is refused as past the end",
     ADAPTER_GUESTMEM,
     """    let last_page_end = file_len.div_ceil(page).saturating_mul(page);""",
     """    let last_page_end = file_len;""",
     PROCFS),
    ("sharedmap-B3", "B", "msync answers MS_INVALIDATE with EINVAL, which Linux accepts",
     ADAPTER_GUESTMEM,
     """    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0""",
     """    if flags & !(MS_ASYNC | MS_SYNC) != 0""",
     PROCFS),
    ("sharedmap-B4", "B", "msync reports ENOMEM over a range that is mapped end to end",
     ADAPTER_GUESTMEM,
     """    let mut hole = from != at || to != end || from >= to;""",
     """    let mut hole = true;""",
     PROCFS),
    # The Java side's web view (WebViewProtocol, ri.a and the page's bridge), 2026-09-23. A rows
    # break what the device does; B rows over-correct in a direction that reads as harmless.
    ("webview-A1", "A", "isAvailable answers its key unquoted by JSONStringer's rules",
     "crates/omni-android/src/jni/webview.rs",
     """format!("{{{}:true}}", json::quote(&self.names.available_key))""",
     """format!("{{\\"{}\\":true}}", self.names.available_key)""",
     WEBVIEW),
    ("webview-A2", "A", "script the engine sends before the page finished runs at once (ri.a.b's queue gone)",
     "crates/omni-android/src/jni/webview.rs",
     """Some(fragment) if fragment.loaded && fragment.window =>""",
     """Some(fragment) if fragment.window =>""",
     WEBVIEW),
    ("webview-A3", "A", "openWindow never binds BrowserService.ExecuteJavaScript (ri.a.h)",
     "crates/omni-android/src/jni/webview.rs",
     """        actions.push(Action::BindExecuteScript);""",
     """""",
     WEBVIEW),
    ("webview-A4", "A", "the host wraps the page's string itself, so the engine gets it wrapped twice",
     "crates/omni-android/src/jni/webview.rs",
     """BrowserEvent::Bridge(text) => vec![Action::Signal(text.clone())],""",
     """BrowserEvent::Bridge(text) => vec![Action::Signal(format!("{{\\"command\\":{}}}", json::quote(text)))],""",
     WEBVIEW),
    ("webview-A5", "A", "the person closing the page never tells the engine (no handleWindowClose)",
     "crates/omni-android/src/jni/webview.rs",
     """                vec![Action::PublishWindowClose]""",
     """                Vec::new()""",
     WEBVIEW),
    ("webview-A6", "A", "a call into a callback object is answered and not queued for the UI thread",
     "crates/omni-android/src/jni/env.rs",
     """            state.host_calls.push(super::HostCall { tag: entry.tag, argument });""",
     """            let _ = (entry.tag, argument);""",
     WEBVIEW),
    ("webview-A7", "A", "messagebus.Connection.<init>(J)V is left unanswered, so doSubscribeRaw's NewObject refuses",
     "crates/omni-android/src/jni/webview.rs",
     """    jni.define(BUS_CONNECTION_CLASS, "<init>", "(J)V", false, Answer::Construct(&[("a", "J")]))?;""",
     """""",
     WEBVIEW),
    ("webview-B1", "B", "a callback object asked for a request's answer returns an empty string",
     "crates/omni-android/src/jni/env.rs",
     """                (_, Some(response)) => Ok(Value::Text(response)),""",
     """                (_, response) => Ok(Value::Text(response.unwrap_or_default())),""",
     WEBVIEW),
    ("webview-B2", "B", "quote leaves '/' bare, as json.org does and Android's JSONStringer does not",
     "crates/omni-android/src/jni/webview.rs",
     r"""                '"' | '\\' | '/' => {""",
     r"""                '"' | '\\' => {""",
     WEBVIEW),
    ("webview-B3", "B", "a repeated key's first value wins, where JSONObject.put keeps the last",
     "crates/omni-android/src/jni/webview.rs",
     """members.iter().rev().find(|(name, _)| name == key)""",
     """members.iter().find(|(name, _)| name == key)""",
     WEBVIEW),
    ("webview-B4", "B", "the user agent's double space before ROBLOX becomes one",
     "crates/omni-android/src/jni/webview.rs",
     """{WEBKIT} (KHTML, like Gecko)  ROBLOX""",
     """{WEBKIT} (KHTML, like Gecko) ROBLOX""",
     WEBVIEW),

    # pthread_condattr_*: the first game join killed a guest thread on `pthread_condattr_init`
    # (2026-09-23), the primitives in omni_bionic::cond written and tested and never wired.
    ("condattr-A1", "A", "pthread_condattr_init is unbound again",
     "crates/omni-android/src/bionic/handlers.rs",
     """    ("pthread_condattr_init", pthread_condattr_init),""",
     """""",
     BIONIC_CONDATTR),
    ("condattr-B1", "B", "pthread_condattr_setclock answers 0 and writes nothing, so the clock is lost",
     "crates/omni-android/src/bionic/handlers.rs",
     """    ("pthread_condattr_setclock", pthread_condattr_setclock),""",
     """    ("pthread_condattr_setclock", pthread_condattr_destroy),""",
     BIONIC_CONDATTR),

    # pthread_attr_setschedparam / pthread_setschedparam: imported, bound before a run reached them
    # (2026-09-23). Each row was also applied by hand once and caught.
    ("setsched-A1", "A", "pthread_setschedparam grants SCHED_FIFO, where an app gets EPERM",
     "crates/omni-android/src/bionic/threads.rs",
     """        FIFO | RR if (1..=99).contains(&priority) => consts::EPERM,""",
     """        FIFO | RR if (1..=99).contains(&priority) => 0,""",
     BIONIC_SCHED),
    ("setsched-A2", "A", "pthread_attr_setschedparam answers 0 and stores nothing",
     "crates/omni-bionic/src/metadata.rs",
     """    mem.write(attr_addr + ATTR_SCHED_PRIORITY, &priority)?;""",
     """    let _ = priority;""",
     BIONIC_SCHED),
    ("setsched-B1", "B", "SCHED_BATCH/SCHED_IDLE are granted silently, so getschedparam would lie",
     "crates/omni-android/src/bionic/threads.rs",
     """        OTHER => 0,
""",
     """        OTHER | BATCH | IDLE => 0,
""",
     BIONIC_SCHED),

    # gethostname: a TaskScheduler worker died on it unbound in a signed-in session (2026-09-23).
    ("gethostname-A1", "A", "a buffer too short for the node name is written anyway, not ENAMETOOLONG",
     "crates/omni-android/src/bionic/procenv.rs",
     """    let fits = usize::try_from(len).is_ok_and(|len| len >= node.len());""",
     """    let fits = true || usize::try_from(len).is_ok_and(|len| len >= node.len());""",
     BIONIC_HOSTNAME),
    ("gethostname-B1", "B", "the node name is copied without its NUL",
     "crates/omni-android/src/bionic/procenv.rs",
     """    node.push(0);
    let fits""",
     """    let fits""",
     BIONIC_HOSTNAME),

    # SystemThemeProtocol.getSystemTheme: a worker died on it unanswered on the Login screen
    # (2026-09-23). Its answer is its decoded body over the uiMode this layer answers.
    ("theme-A1", "A", "getSystemTheme reads the night mode from the wrong bits of uiMode",
     "crates/omni-android/src/jni/classes.rs",
     """    match ui_mode & 0x30 {""",
     """    match ui_mode & 0x0f {""",
     JNI_THEME),
    ("theme-B1", "B", "getSystemTheme answered as a constant, not from the uiMode the field answers",
     "crates/omni-android/src/jni/classes.rs",
     """            s("getSystemTheme", "()I", Answer::Int(system_theme_for(UI_MODE))),""",
     """            s("getSystemTheme", "()I", Answer::Int(4)),""",
     JNI_THEME),

    # getaddrinfo with an empty service: what pressing Play reached next (2026-09-23).
    ("gai-A1", "A", "an empty getaddrinfo service is refused again, as a service name",
     "crates/omni-android/src/bionic/net.rs",
     """        if bytes.is_empty() {
            // **An empty service""",
     """        if false {
            // **An empty service""",
     BIONIC_GAI),
    ("gai-B1", "B", "AI_NUMERICSERV is ignored for an empty service: EAI_SERVICE where bionic says EAI_NONAME",
     "crates/omni-android/src/bionic/net.rs",
     """            return Ok(if hints.flags & AI_NUMERICSERV != 0 { net::EAI_NONAME } else { EAI_SERVICE });""",
     """            return Ok(EAI_SERVICE);""",
     BIONIC_GAI),

    # The futex compares its word, as FUTEX_WAIT does (the performance subagent, 2026-09-23): an
    # unlock between a waiter marking the mutex and parking used to wake nobody, and the waiter
    # slept out mutex::contend's 1,000 ms slice (MEASURED once on the render thread).
    ("futexcmp-A1", "A", "AddressFutex::wait parks without comparing the word again",
     "crates/omni-android/src/bionic/runtime.rs",
     """        let still_expected = || !aligned || unsafe { word_holds(addr, expected) };""",
     """        let still_expected = || { let _ = aligned; true };""",
     FUTEX_RUNTIME),
    ("futexcmp-B1", "B", "a RECURSIVE mutex waits on LOCKED_WITH_WAITERS, which its word never holds: a spin",
     "crates/omni-bionic/src/mutex.rs",
     """        let expected = if type_ != mutex_type::RECURSIVE && state == lock_state::LOCKED {""",
     """        let expected = if state == lock_state::LOCKED || type_ == mutex_type::RECURSIVE {""",
     BIONIC_MUTEX_LIB),

    # getaddrinfo with a null node: what Play reached after the empty service (2026-09-23).
    ("nullnode-A1", "A", "a null node ignores AI_PASSIVE and answers loopback, not the bind address",
     "crates/omni-android/src/bionic/net.rs",
     """                match (*family, hints.flags & AI_PASSIVE != 0) {""",
     """                match (*family, false) {""",
     BIONIC_GAI),
    ("nullnode-B1", "B", "a null node's AF_UNSPEC answer puts IPv4 first, where bionic's explore table has IPv6",
     "crates/omni-android/src/bionic/net.rs",
     """            None => &[platnet::IpFamily::V6, platnet::IpFamily::V4],""",
     """            None => &[platnet::IpFamily::V4, platnet::IpFamily::V6],""",
     BIONIC_GAI),

    # FD_CLOEXEC, recorded where descriptors are made and read by fcntl(F_GETFD): what Play reached
    # after the null node (2026-09-23).
    ("cloexec-A1", "A", "record_close_on_exec records nothing: every creator's flag is lost",
     "crates/omni-android/src/bionic/files.rs",
     """    if on {
        if let Settled::Failed(errno) = settle(view, fs.set_close_on_exec(fd, true))? {""",
     """    if false {
        if let Settled::Failed(errno) = settle(view, fs.set_close_on_exec(fd, true))? {""",
     BIONIC_CLOEXEC),
    ("cloexec-A2", "A", "open drops O_CLOEXEC",
     "crates/omni-android/src/bionic/files.rs",
     """        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, flags & O_CLOEXEC != 0)?,""",
     """        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, false)?,""",
     BIONIC_CLOEXEC),
    ("cloexec-A3", "A", "__open_2 drops O_CLOEXEC",
     "crates/omni-android/src/bionic/files.rs",
     """        }
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, flags & O_CLOEXEC != 0)?,""",
     """        }
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, false)?,""",
     BIONIC_CLOEXEC),
    ("cloexec-A4", "A", "F_GETFD answers 0 for a descriptor that carries FD_CLOEXEC",
     "crates/omni-android/src/bionic/files.rs",
     """                Settled::Done(true) => FD_CLOEXEC,""",
     """                Settled::Done(true) => 0,""",
     BIONIC_CLOEXEC),
    ("cloexec-A5", "A", "fopen drops its mode's `e`",
     "crates/omni-android/src/bionic/stdio.rs",
     """            Ok(mode) => (open_flags(mode), mode.close_on_exec),""",
     """            Ok(mode) => (open_flags(mode), false),""",
     BIONIC_CLOEXEC),
    ("cloexec-A6", "A", "socket drops SOCK_CLOEXEC",
     "crates/omni-android/src/bionic/net.rs",
     """                super::files::record_close_on_exec(&view, fs, fd, flags & SOCK_CLOEXEC != 0)?""",
     """                super::files::record_close_on_exec(&view, fs, fd, false)?""",
     BIONIC_CLOEXEC),
    ("cloexec-A7", "A", "epoll_create1 drops EPOLL_CLOEXEC",
     "crates/omni-android/src/bionic/net.rs",
     """                    super::files::record_close_on_exec(&view, fs, fd, flags & EPOLL_CLOEXEC != 0)?""",
     """                    super::files::record_close_on_exec(&view, fs, fd, false)?""",
     BIONIC_CLOEXEC),
    ("cloexec-A8", "A", "eventfd drops EFD_CLOEXEC",
     "crates/omni-platform/src/fs/mod.rs",
     """        if flags & eventfd::EFD_CLOEXEC != 0 {""",
     """        if false {""",
     BIONIC_CLOEXEC),
    ("cloexec-A9", "A", "timerfd_create drops TFD_CLOEXEC",
     "crates/omni-platform/src/fs/mod.rs",
     """        if flags & timerfd::TFD_CLOEXEC != 0 {""",
     """        if false {""",
     BIONIC_CLOEXEC),
    ("cloexec-B1", "B", "close leaves the flag on the number, so the next open that reuses it inherits it",
     "crates/omni-platform/src/fs/mod.rs",
     """        table.close_on_exec.remove(&fd);
        match table.open.remove(&fd) {""",
     """        match table.open.remove(&fd) {""",
     BIONIC_CLOEXEC),
    ("cloexec-B2", "B", "F_SETFD(0) cannot clear the flag",
     "crates/omni-platform/src/fs/mod.rs",
     """        } else {
            table.close_on_exec.remove(&fd);
        }""",
     """        } else {
        }""",
     BIONIC_CLOEXEC),
    ("cloexec-B3", "B", "F_SETFD keeps any nonzero argument rather than only FD_CLOEXEC of it",
     "crates/omni-android/src/bionic/files.rs",
     """                let on = argument as i32 & FD_CLOEXEC != 0;""",
     """                let on = argument != 0;""",
     BIONIC_CLOEXEC),

    # SO_LINGER and SO_BROADCAST: the engine's game-socket setup, what Play reached after F_GETFD
    # (2026-09-23).
    ("linger-A1", "A", "a datagram socket's SO_LINGER goes to the host, which refuses it (the death)",
     "crates/omni-platform/src/net/mod.rs",
     """                Inner::Udp(_) => {
                    self.kept.linger = linger;
                    Ok(())
                }""",
     """                Inner::Udp(_) => backend::set_linger(&self.inner, linger.is_some()),""",
     BIONIC_LINGER),
    ("linger-A2", "A", "a stream socket's SO_BROADCAST goes to the host, which refuses it",
     "crates/omni-platform/src/net/mod.rs",
     """                Inner::Tcp(_) => {
                    self.kept.broadcast = on;
                    Ok(())
                }""",
     """                Inner::Tcp(_) => backend::set_broadcast(&self.inner, on),""",
     BIONIC_LINGER),
    ("linger-A3", "A", "a short struct linger is read past its optlen instead of EINVAL",
     "crates/omni-android/src/bionic/net.rs",
     """                if given < LINGER_BYTES {""",
     """                if given < 4 {""",
     BIONIC_LINGER),
    ("linger-A4", "A", "a nonzero linger on a stream socket reaches the platform and answers ENOPROTOOPT, not a refusal",
     "crates/omni-android/src/bionic/net.rs",
     """                    } else if seconds != 0
                        && locked(&handle).kind() == omni_platform::net::SocketKind::Stream""",
     """                    } else if false""",
     BIONIC_LINGER),
    ("linger-B1", "B", "l_onoff read inverted: off is on and on is off",
     "crates/omni-android/src/bionic/net.rs",
     """                    if !on {
                        // Off: Linux ignores `l_linger` then.""",
     """                    if on {
                        // Off: Linux ignores `l_linger` then.""",
     BIONIC_LINGER),
    ("linger-B2", "B", "the abortive close sets linger off on the host",
     "crates/omni-platform/src/net/mod.rs",
     """                    Some(time) if time.is_zero() => backend::set_linger(&self.inner, true),""",
     """                    Some(time) if time.is_zero() => backend::set_linger(&self.inner, false),""",
     PLAT_LINGER),
    ("linger-B3", "B", "a datagram socket's SO_BROADCAST is kept instead of set on the host",
     "crates/omni-platform/src/net/mod.rs",
     """                Inner::Udp(_) => backend::set_broadcast(&self.inner, on),""",
     """                Inner::Udp(_) => {
                    self.kept.broadcast = on;
                    Ok(())
                }""",
     PLAT_LINGER),

    # Path-MTU discovery by mode, and AT_SECURE: what RakNet's join reached (2026-09-23).
    ("pmtu-A1", "A", "IP_PMTUDISC_PROBE is refused again (the death)",
     "crates/omni-android/src/bionic/net.rs",
     """                    Some(IP_PMTUDISC_PROBE) => Some(SocketOption::PathMtuDiscovery(PathMtu::Probe)),""",
     """                    Some(99) => Some(SocketOption::PathMtuDiscovery(PathMtu::Probe)),""",
     BIONIC_PMTU),
    ("pmtu-A2", "A", "PROBE reaches Windows as DO",
     "crates/omni-platform/src/net/windows.rs",
     """        PathMtu::Probe => IP_PMTUDISC_PROBE,""",
     """        PathMtu::Probe => IP_PMTUDISC_DO,""",
     PLAT_PMTU),
    ("pmtu-B1", "B", "the guest's IP_PMTUDISC_DONT becomes DO",
     "crates/omni-android/src/bionic/net.rs",
     """                    Some(IP_PMTUDISC_DONT) => Some(SocketOption::PathMtuDiscovery(PathMtu::Dont)),""",
     """                    Some(IP_PMTUDISC_DONT) => Some(SocketOption::PathMtuDiscovery(PathMtu::Do)),""",
     BIONIC_PMTU),
    ("pmtu-B2", "B", "the host's NOT_SET default reads back as DONT",
     "crates/omni-platform/src/net/windows.rs",
     """        IP_PMTUDISC_NOT_SET => Ok(None),""",
     """        IP_PMTUDISC_NOT_SET => Ok(Some(PathMtu::Dont)),""",
     PLAT_PMTU),
    ("pmtu-B3", "B", "the other family's level is accepted and applied at the socket's own",
     "crates/omni-android/src/bionic/net.rs",
     """                    Some(mode) if !family_matches => {""",
     """                    Some(mode) if false && !family_matches => {""",
     BIONIC_PMTU),
    ("atsecure-A1", "A", "AT_SECURE answers 1: a privileged-exec claim nothing made",
     "crates/omni-android/src/bionic/procenv.rs",
     """        AT_SECURE => 0,""",
     """        AT_SECURE => 1,""",
     BIONIC_AUXV),
    ("atsecure-A2", "A", "AT_SECURE is refused again (the death)",
     "crates/omni-android/src/bionic/procenv.rs",
     """        AT_SECURE => 0,
""",
     """""",
     BIONIC_AUXV),

    # sincos: the in-game worker pool of the first join to connect (2026-09-23).
    ("sincos-A1", "A", "sincos writes the cosine where the sine goes",
     "crates/omni-bionic/src/libm.rs",
     """        (x.sin(), x.cos())
    };
    if sin_ptr != 0 {
        ctx.write(sin_ptr, &s.to_le_bytes())?;
    }
    if cos_ptr != 0 {
        ctx.write(cos_ptr, &c.to_le_bytes())?;
    }
    Ok(())
}

/// `void sincosf""",
     """        (x.cos(), x.sin())
    };
    if sin_ptr != 0 {
        ctx.write(sin_ptr, &s.to_le_bytes())?;
    }
    if cos_ptr != 0 {
        ctx.write(cos_ptr, &c.to_le_bytes())?;
    }
    Ok(())
}

/// `void sincosf""",
     LIBM_SINCOS),
    ("sincos-A2", "A", "sincos writes single-precision results, four bytes each",
     "crates/omni-bionic/src/libm.rs",
     """        (f64::NAN, f64::NAN)
    } else {
        (x.sin(), x.cos())
    };""",
     """        (f64::NAN, f64::NAN)
    } else {
        ((x as f32).sin() as f64, (x as f32).cos() as f64)
    };""",
     LIBM_SINCOS),
    ("sincos-B1", "B", "an infinite x sets no errno",
     "crates/omni-bionic/src/libm.rs",
     """    let (s, c) = if x.is_infinite() {
        ctx.set_errno(EDOM);
        (f64::NAN, f64::NAN)""",
     """    let (s, c) = if x.is_infinite() {
        (f64::NAN, f64::NAN)""",
     LIBM_SINCOS),

    # A loaded world's capacity (the first joins to load Pet Simulator 99's world, 2026-09-23).
    ("capacity-A1", "A", "the thread table back to 64 blocks",
     "crates/omni-android/src/bionic/mod.rs",
     "pub const MAX_GUEST_THREADS: usize = 256;",
     "pub const MAX_GUEST_THREADS: usize = 64;",
     CAPACITY_BIONIC),
    ("capacity-A2", "A", "the stream table back to 16 FILEs",
     "crates/omni-android/src/bionic/mod.rs",
     "pub const MAX_GUEST_FILES: usize = 512;",
     "pub const MAX_GUEST_FILES: usize = 16;",
     CAPACITY_BIONIC),
    ("capacity-A3", "A", "the JNIEnv table back to 64, apart from bionic's thread table",
     "crates/omni-android/src/jni/mod.rs",
     "pub const MAX_JNI_THREADS: usize = crate::bionic::MAX_GUEST_THREADS;",
     "pub const MAX_JNI_THREADS: usize = 64;",
     CAPACITY_JNI),
    ("capacity-A4", "A", "the arena's stated granules understate its eager commit",
     "crates/omni-android/src/bionic/mod.rs",
     "pub const ARENA_GRANULES: usize = 5;",
     "pub const ARENA_GRANULES: usize = 4;",
     CAPACITY_ARENA),
    ("capacity-B1", "B", "the arena's stated granules overstate its eager commit",
     "crates/omni-android/src/bionic/mod.rs",
     "pub const ARENA_GRANULES: usize = 5;",
     "pub const ARENA_GRANULES: usize = 6;",
     CAPACITY_ARENA),

    # vkCmdCopyImageToBuffer: the render thread's read-back on a loaded world's first frame.
    ("readback-A1", "A", "vkCmdCopyImageToBuffer falls back to the refusal",
     "crates/omni-android/src/vulkan/mod.rs",
     """        "vkCmdCopyImageToBuffer" => draw::cmd_copy_image_to_buffer(c, &at, &vulkan, args),
""",
     "",
     VULKAN_READBACK),
    ("readback-A2", "A", "the image and the buffer swapped",
     "crates/omni-android/src/vulkan/draw.rs",
     """    let image = vulkan.image_ref_token(at, CALL, args[1])?;
    let destination = vulkan.buffer_token(at, CALL, args[3])?;
    let regions = read_regions(
        c,
        at,
        CALL,
        "pRegions",
        "VkBufferImageCopy",""",
     """    let image = vulkan.image_ref_token(at, CALL, args[3])?;
    let destination = vulkan.buffer_token(at, CALL, args[1])?;
    let regions = read_regions(
        c,
        at,
        CALL,
        "pRegions",
        "VkBufferImageCopy",""",
     VULKAN_READBACK),
    ("readback-A3", "A", "the layout read from the wrong register",
     "crates/omni-android/src/vulkan/draw.rs",
     "host.cmd_copy_image_to_buffer(buffer, image, args[2] as u32, destination, &regions)?;",
     "host.cmd_copy_image_to_buffer(buffer, image, args[4] as u32, destination, &regions)?;",
     VULKAN_READBACK),
    ("readback-B1", "B", "the region count taken as one rather than the guest's",
     "crates/omni-android/src/vulkan/draw.rs",
     """        BUFFER_IMAGE_COPY_BYTES,
        args[4],
        args[5],
        5,
    )?;
    host.cmd_copy_image_to_buffer(""",
     """        BUFFER_IMAGE_COPY_BYTES,
        1,
        args[5],
        5,
    )?;
    host.cmd_copy_image_to_buffer(""",
     VULKAN_READBACK),

    # ---- keyboard and mouse: `jni::mouse` (vk.e's mouse half), `jni::keys` focus loss, and the
    # window seam's wheel and raw motion. Rows `mouse-*` and `kbd-*`. Each one is a decoded fact a
    # wrong constant would break; the detectors are `jni::mouse`'s tests, `tests/input.rs`'s probes
    # (real translated code, the registers each native reads) and the window seam's unit tests.
    #
    # **Positions in pixels where the engine reads dp.** `vk.e.y` divides by `q()` (`0x006a`); at
    # this host's density of 1.0 no run could see it. The detectors are at 1.5.
    ("mouse-A1", "A", "a mouse move reaches the engine in pixels instead of dp",
     "crates/omni-android/src/jni/mouse.rs",
     """                let x = event.x / self.density;
                let y = event.y / self.density;""",
     """                let x = event.x;
                let y = event.y;""",
     INPUT),

    # **The motion as the position.** `dx = x - m` (`0x007c`): the camera turns on dx/dy.
    ("mouse-A2", "A", "a move's motion is its position rather than its difference from the last",
     "crates/omni-android/src/jni/mouse.rs",
     """                let (dx, dy) = (x - self.m, y - self.n);""",
     """                let (dx, dy) = (x, y);""",
     INPUT),

    # **`getActionButton()` without the `- 1`** (`0x0025`): left would be the engine's right.
    ("mouse-A3", "A", "the button is the action button rather than the action button less one",
     "crates/omni-android/src/jni/mouse.rs",
     """                button: event.action_button - 1,""",
     """                button: event.action_button,""",
     INPUT),

    # **A press at the event's position.** `y` presses at `m`/`n`, the last move (`0x001d`), which a
    # captured press (position 0, 0) shows.
    ("mouse-A4", "A", "a press is sent at the event's position rather than the last move's",
     "crates/omni-android/src/jni/mouse.rs",
     """            MouseAction::ButtonPress | MouseAction::ButtonRelease => calls.push(MouseCall::Button {
                x: self.m,
                y: self.n,""",
     """            MouseAction::ButtonPress | MouseAction::ButtonRelease => calls.push(MouseCall::Button {
                x: event.x / self.density,
                y: event.y / self.density,""",
     INPUT),

    # **The wheel's position not clamped at zero** (`0x0048`-`0x0057`, `cmpl-float`/`if-lez`).
    ("mouse-A5", "A", "the wheel is sent at a negative position",
     "crates/omni-android/src/jni/mouse.rs",
     """                let positive = |v: f32| if v > 0.0 { v } else { 0.0 };""",
     """                let positive = |v: f32| v;""",
     INPUT),

    # **The host's wheel units as notches.** `AXIS_VSCROLL` is one per notch; Windows' is 120.
    ("mouse-A6", "A", "a wheel notch reaches the engine as 120 notches",
     "crates/omni-android/src/jni/mouse.rs",
     """                scroll.vscroll = dy as f32 / WHEEL_DELTA;""",
     """                scroll.vscroll = dy as f32;""",
     INPUT),

    # **The lock's request inverted** (`vk.e$e` `0x00b0`-`0x00c3`): locked and not captured asks.
    ("mouse-A7", "A", "a locked engine is not given the pointer capture",
     "crates/omni-android/src/jni/mouse.rs",
     """                if locked()? && !has_capture {""",
     """                if locked()? && has_capture {""",
     INPUT),

    # **The release inverted** (`vk.e$d` `0x0000`-`0x0014`): unlocked and captured gives it back.
    ("mouse-A8", "A", "an unlocked engine keeps the pointer captured",
     "crates/omni-android/src/jni/mouse.rs",
     """                if !locked()? && has_capture {""",
     """                if !locked()? && !has_capture {""",
     INPUT),

    # **The captured position not accumulated** -- `AndroidMouseLockButtonFix` taken as on, where
    # its compiled default is off (`di/a.<init>` `0x0c14`).
    ("mouse-A9", "A", "a captured move does not accumulate the position",
     "crates/omni-android/src/jni/mouse.rs",
     """                self.m += dx;
                self.n += dy;""",
     """                let _ = (dx, dy);""",
     INPUT),

    # **Relative motion in counts where the engine reads dp** (`vk.e.z` `0x001a`, `0x0025`).
    ("mouse-A10", "A", "a captured move reaches the engine in counts instead of dp",
     "crates/omni-android/src/jni/mouse.rs",
     """                let dx = event.relative.0 / self.density;
                let dy = event.relative.1 / self.density;""",
     """                let dx = event.relative.0;
                let dy = event.relative.1;""",
     INPUT),

    # **A press without its move**: the engine is told the click happened where the pointer last
    # was -- what a synthetic tap aimed at a button would do.
    ("mouse-A11", "A", "a press that arrives without its move is not moved there first",
     "crates/omni-android/src/jni/mouse.rs",
     """                if !captured {
                    self.move_to(x, y, &mut out);
                }
                self.press(bit, captured, &mut out);""",
     """                self.press(bit, captured, &mut out);""",
     INPUT),

    # **The middle button "repaired"**: `TERTIARY` is 4, so the Java side sends 3, which the engine
    # makes `None` (`0x2e4d3ac`). Sending 2 would be a device that does not exist.
    ("mouse-A12", "A", "the middle button is sent as MouseButton3",
     "crates/omni-android/src/jni/mouse.rs",
     """        PointerButton::Middle => BUTTON_TERTIARY,""",
     """        PointerButton::Middle => 3,""",
     INPUT),

    # **Down and the button in each other's registers** (`w2`, `w3`; `0x02bbbd94`, `0x02bbbd98`).
    ("mouse-A13", "A", "the button's down flag and index trade registers",
     "crates/omni-android/src/jni/mouse.rs",
     """            GuestArg::Int(u64::from(down)),
            GuestArg::Int(i64::from(button) as u64),""",
     """            GuestArg::Int(i64::from(button) as u64),
            GuestArg::Int(u64::from(down)),""",
     INPUT),

    # **The position and the motion in each other's registers** (`s0`/`s1` x, y; `s2`/`s3` dx, dy).
    ("mouse-A14", "A", "a move's position and motion trade registers",
     "crates/omni-android/src/jni/mouse.rs",
     """            GuestArg::Float(x),
            GuestArg::Float(y),
            GuestArg::Float(dx),
            GuestArg::Float(dy),""",
     """            GuestArg::Float(dx),
            GuestArg::Float(dy),
            GuestArg::Float(x),
            GuestArg::Float(y),""",
     INPUT),

    # **Held buttons kept through a lost focus**: a right button the engine holds for ever is a
    # camera that never stops turning.
    ("mouse-A15", "A", "losing the focus releases only the back and forward buttons",
     "crates/omni-android/src/jni/mouse.rs",
     """                for bit in [BUTTON_PRIMARY, BUTTON_SECONDARY, BUTTON_TERTIARY, BUTTON_BACK, BUTTON_FORWARD] {""",
     """                for bit in [BUTTON_BACK, BUTTON_FORWARD] {""",
     INPUT),

    # **A capture the window lost, believed held**: the listener would never ask again.
    ("mouse-A16", "A", "a lost pointer capture is not taken as lost",
     "crates/omni-android/src/jni/mouse.rs",
     """        if matches!(event, WindowEvent::PointerCaptureLost) {
            self.captured = false;
        }""",
     """        let _ = matches!(event, WindowEvent::PointerCaptureLost);""",
     INPUT),

    # **A captured press routed as a generic one**: it would ask the engine for a capture the view
    # already holds instead of reaching `vk.e.z`.
    ("mouse-A17", "A", "a captured event is routed to the generic-motion listener",
     "crates/omni-android/src/jni/mouse.rs",
     """        let route = if captured {
            Route::Captured
        } else if matches!(action, MouseAction::Down | MouseAction::Move | MouseAction::Up) {""",
     """        let route = if captured {
            Route::Generic
        } else if matches!(action, MouseAction::Down | MouseAction::Move | MouseAction::Up) {""",
     INPUT),

    # **The wheel's sign lost**: `WM_MOUSEWHEEL`'s high word is signed, so a notch towards the user
    # read unsigned is 65,416.
    ("mouse-B1", "A", "the wheel delta is read unsigned",
     "crates/omni-platform/src/window/windows.rs",
     """    ((wparam >> 16) & 0xffff) as u16 as i16 as i32""",
     """    ((wparam >> 16) & 0xffff) as u16 as i32""",
     ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast", "window::"]),

    # **Absolute raw input taken as relative**: a remote-desktop mouse would teleport the camera.
    ("mouse-B2", "A", "absolute raw input is taken as relative motion",
     "crates/omni-platform/src/window/windows.rs",
     """    if flags & MOUSE_MOVE_ABSOLUTE == 0 {""",
     """    if flags & MOUSE_MOVE_ABSOLUTE != 0 {""",
     ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast", "window::"]),

    # **The first absolute report taken as motion from the origin.**
    ("mouse-B3", "A", "the first absolute raw report is motion from the corner",
     "crates/omni-platform/src/window/windows.rs",
     """        None => (0, 0),""",
     """        None => now,""",
     ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast", "window::"]),

    # **Relative motion replaced, not summed**, when a run of it is coalesced: every sample but the
    # last between two polls lost.
    ("mouse-B4", "A", "coalesced relative motion keeps only the newest sample",
     "crates/omni-platform/src/window/mod.rs",
     """        *sum_x = sum_x.saturating_add(*dx);
        *sum_y = sum_y.saturating_add(*dy);""",
     """        *sum_x = *dx;
        *sum_y = *dy;""",
     ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast", "window::"]),

    # **The two wheel messages' axes swapped.**
    ("mouse-B5", "A", "the vertical wheel is reported as the horizontal one",
     "crates/omni-platform/src/window/windows.rs",
     """    if msg == WM_MOUSEHWHEEL { (delta, 0) } else { (0, delta) }""",
     """    if msg == WM_MOUSEWHEEL { (delta, 0) } else { (0, delta) }""",
     ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast", "window::"]),

    # **Keys held through a lost focus are not released**: Windows sends no key-up to a window
    # without the focus, and Android's dispatcher cancels every held key when the focus leaves.
    ("kbd-A1", "A", "losing the focus releases no held key",
     "crates/omni-android/src/jni/keys.rs",
     """            (WindowEvent::FocusChanged { focused: false }, _) => {""",
     """            (WindowEvent::FocusChanged { focused: true }, _) => {""",
     INPUT),

    # **A released key still held**: released again at the next focus loss, and an auto-repeat
    # held twice.
    ("kbd-A2", "A", "a key that came up is still believed held",
     "crates/omni-android/src/jni/keys.rs",
     """            self.held.retain(|key| key.scan_code != call.scan_code);""",
     """            self.held.retain(|_| true);""",
     INPUT),

    # **A cancel carrying the last press's repeat flag**: a cancel's repeat count is 0.
    ("kbd-A3", "A", "a cancelling release is sent as an auto-repeat",
     "crates/omni-android/src/jni/keys.rs",
     """                    .map(|key| PassKeyEvent { down: false, repeat: false, ..*key })""",
     """                    .map(|key| PassKeyEvent { down: false, ..*key })""",
     INPUT),
    # Inbound sockets and NetworkUtils: the MicroProfiler web server that froze a game world
    # (2026-09-23). Two rows are deliberately absent -- dropping `accept`'s not-listening shortcut
    # and its SO_RCVTIMEO deadline -- because the tests that catch them would HANG, not fail.
    ("inbound-A1", "A", "a loopback-only policy admits listening on any address",
     "crates/omni-platform/src/net/policy.rs",
     "if self.unrestricted || self.listen || (self.loopback && local.is_loopback()) {",
     "if self.unrestricted || self.listen || self.loopback {",
     INBOUND_PLAT),
    ("inbound-A2", "A", "listen skips the policy",
     "crates/omni-platform/src/net/mod.rs",
     """        self.policy.check_listen(OP, &local)?;
""",
     "",
     INBOUND_PLAT),
    ("inbound-A3", "A", "an unbound socket is judged as loopback rather than the wildcard",
     "crates/omni-platform/src/net/mod.rs",
     "            return Ok(SocketAddress::unspecified(self.family));",
     "            return Ok(SocketAddress::loopback(self.family, 0));",
     INBOUND_PLAT),
    ("inbound-A4", "A", "the accepted socket keeps Winsock's inherited non-blocking mode",
     "crates/omni-platform/src/net/mod.rs",
     """        stream.set_nonblocking(false).map_err(|error| NetError::io(OP, peer.to_string(), &error))?;
""",
     "",
     INBOUND_PLAT),
    ("inbound-A5", "A", "the host's IPv4 addresses are skipped",
     "crates/omni-platform/src/net/windows.rs",
     "if family == AF_INET && length >= core::mem::size_of::<SOCKADDR_IN>() {",
     "if false && family == AF_INET && length >= core::mem::size_of::<SOCKADDR_IN>() {",
     INBOUND_PLAT),
    ("inbound-A6", "A", "listen on a datagram socket reaches the seam instead of EOPNOTSUPP",
     "crates/omni-android/src/bionic/net.rs",
     "        if socket.kind() == SocketKind::Stream {\n            match settled(&view, socket.listen(backlog))? {",
     "        if true {\n            match settled(&view, socket.listen(backlog))? {",
     INBOUND_BIONIC),
    ("inbound-A7", "A", "accept writes no peer address",
     "crates/omni-android/src/bionic/net.rs",
     """                    write_peer(&view, addr, addrlen, &peer)?;
                    new_fd""",
     """                    new_fd""",
     INBOUND_BIONIC),
    ("inbound-A8", "A", "getPublicIPv4Addresseses keeps loopback",
     "crates/omni-android/src/jni/classes.rs",
     """        if address.is_loopback() {
            continue;
        }
        let text = address.to_string();""",
     """        let text = address.to_string();""",
     INBOUND_JNI),
    ("inbound-A9", "A", "getPublicIPv4Addresseses keeps IPv6",
     "crates/omni-android/src/jni/classes.rs",
     """        if text.contains(':') {
            continue;
        }""",
     "",
     INBOUND_JNI),
    ("inbound-B1", "B", "getPublicIPv4Addresseses drops the trailing separator Java leaves",
     "crates/omni-android/src/jni/classes.rs",
     """        result.push_str(&text);
        result.push_str(" : ");
    }
    result""",
     """        if !result.is_empty() {
            result.push_str(" : ");
        }
        result.push_str(&text);
    }
    result""",
     INBOUND_JNI),
    # `__vsprintf_chk`: a TaskScheduler worker died on it unbound in place 606849621 and the game
    # froze (2026-09-23). bionic's fortify.cpp: vsnprintf into dest_len, then the check.
    ("vsprintf-A1", "A", "__vsprintf_chk is left unbound",
     "crates/omni-android/src/bionic/handlers.rs",
     """    ("__vsprintf_chk", format::vsprintf_chk),
""",
     "",
     VSPRINTF),
    ("vsprintf-A2", "A", "a result that does not fit is not the fortify fatal",
     "crates/omni-android/src/bionic/format.rs",
     "        if claim > dest_len {",
     "        if false && claim > dest_len {",
     VSPRINTF),
    ("vsprintf-A3", "A", "the claim leaves out the terminator",
     "crates/omni-android/src/bionic/format.rs",
     "let claim = u64::try_from(result).map_or(0, |result| result + 1);",
     "let claim = u64::try_from(result).map_or(0, |result| result);",
     VSPRINTF),
    ("vsprintf-A4", "A", "nothing is written to the destination",
     "crates/omni-android/src/bionic/format.rs",
     "let result = write_truncated(&view, destination, dest_len, &text, 0)?;",
     "let result = write_truncated(&view, destination, 0, &text, 0)?;",
     VSPRINTF),
    ("vsprintf-B1", "B", "a result of exactly dest_len - 1 characters is refused",
     "crates/omni-android/src/bionic/format.rs",
     "        if claim > dest_len {",
     "        if claim >= dest_len {",
     VSPRINTF),
    ("vsprintf-B2", "B", "the write before the fatal is not truncated to dest_len",
     "crates/omni-android/src/bionic/format.rs",
     "let result = write_truncated(&view, destination, dest_len, &text, 0)?;",
     "let result = write_truncated(&view, destination, u64::MAX, &text, 0)?;",
     VSPRINTF),
    # A death recorded as the native crash it is (2026-09-24). What only a live run can see -- the
    # watchdog's halt of the UI thread's call, and the record site -- was verified by the injected
    # runs I1/I2 (docs/ports/windows.md), not by a row.
    ("crashclose-A1", "A", "a native crash is handed to the engine under another reason's name",
     "crates/omni-android/src/jni/mod.rs",
     'Self::REASON_CRASH_NATIVE => Ok("APP CRASH(NATIVE)"),',
     'Self::REASON_CRASH_NATIVE => Ok("CRASH_NATIVE"),',
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "previous_exits"]),
    ("crashclose-A2", "A", "a fault this layer caught is recorded as an abort",
     "crates/omni-android/tests/gameactivity.rs",
     """    if why.starts_with("MemoryFault") {
        omni_android::jni::ExitRecord::SIGSEGV""",
     """    if why.starts_with("MemoryFault") {
        omni_android::jni::ExitRecord::SIGABRT""",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "gameactivity", "--no-fail-fast",
      "a_death_is_recorded"]),
    ("crashclose-A3", "A", "an unexecutable instruction is recorded as an abort",
     "crates/omni-android/tests/gameactivity.rs",
     """    } else if why.starts_with("UnsupportedInstruction") {""",
     """    } else if why.starts_with("UnsupportedInstructionX") {""",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "gameactivity", "--no-fail-fast",
      "a_death_is_recorded"]),
    # A guest's raw `SVC #0`, answered by the syscall emulation in the kernel's convention, and
    # `atol` -- the two deaths of the owner's first join of place 606849621 (2026-09-24).
    ("svc-A1", "A", "a raw SVC #0 stops the thread as an unsupported instruction again",
     "crates/omni-android/src/boundary.rs",
     "ExitReason::UnsupportedInstruction { pc: at, encoding: SVC_0 } =>",
     "ExitReason::UnsupportedInstruction { pc: at, encoding: 0xFFFF_FFFF } =>",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "raw_svc"]),
    ("svc-A2", "A", "the guest's errno is left holding the call's error",
     "crates/omni-android/src/boundary.rs",
     "        self.mem.write_u32(errno_at, saved, blame)?;",
     "",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "raw_svc"]),
    ("svc-A3", "A", "a failed raw syscall answers libc's -1 rather than -errno",
     "crates/omni-android/src/boundary.rs",
     "        let result = if answer as i64 == -1 {",
     "        let result = if false {",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "raw_svc"]),
    ("svc-A4", "A", "a raw openat relative to a descriptor is resolved against the working directory",
     "crates/omni-android/src/boundary.rs",
     "            if dirfd != AT_FDCWD && !absolute {",
     "            if false {",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "raw_svc"]),
    ("svc-B1", "B", "the kernel's registers are taken in libc's order, number in x0",
     "crates/omni-android/src/boundary.rs",
     '            ("syscall", &[8, 0, 1, 2, 3, 4, 5])',
     '            ("syscall", &[0, 1, 2, 3, 4, 5, 6])',
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "raw_svc"]),
    ("atol-A1", "A", "atol is left unbound",
     "crates/omni-android/src/bionic/handlers.rs",
     """    ("atol", atol),
""",
     "",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "atol"]),
    ("atol-B1", "B", "atol truncates to an int as atoi does",
     "crates/omni-android/src/bionic/handlers.rs",
     "    fn atol(s: ptr) -> u64 = |v| omni_bionic::numerics::atoll(&mut v, s).map(|n| n as u64);",
     "    fn atol(s: ptr) -> u64 = |v| omni_bionic::numerics::atoi(&mut v, s).map(|n| i64::from(n) as u64);",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "atol"]),
    # The app's cookie store (2026-09-24, the owner's decision): what Android's CookieManager keeps
    # for the engine, and the startup call that hands it back -- a sign-in surviving a restart.
    ("cookie-A1", "A", "onSetCookie drops the engine's cookies again",
     "crates/omni-android/src/jni/classes.rs",
     'methods: &[m("onSetCookie", "([Ljava/lang/String;Ljava/lang/String;)V", Answer::OnSetCookie)],',
     'methods: &[m("onSetCookie", "([Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink)],',
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    ("cookie-A2", "A", "nativeSetMultipleCookies is handed nothing at startup",
     "crates/omni-android/src/jni/script.rs",
     "ScriptArg::CookiesFor(url) => GuestArg::Int(jni.new_string(&jni.cookie_header(url))?),",
     "ScriptArg::CookiesFor(_url) => GuestArg::Int(jni.new_string(\"\")?),",
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    ("cookie-A3", "A", "the store is never written back to its file",
     "crates/omni-android/src/jni/cookies.rs",
     "        std::fs::rename(&partial, file).map_err(fail)",
     "        let _ = (partial, fail); Ok(())",
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    ("cookie-A4", "A", "an expired cookie is stored rather than deleting the one it names",
     "crates/omni-android/src/jni/cookies.rs",
     "        if expires_ms.is_none_or(|at| at > now_ms) {",
     "        if true {",
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    ("cookie-A5", "A", "a Domain the URL's host does not match is accepted",
     "crates/omni-android/src/jni/cookies.rs",
     "            Some(domain) if domain_matches(&url.host, &domain) => (domain, false),",
     "            Some(domain) => (domain, false),",
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    ("cookie-B1", "B", "a secure cookie is sent over plain http",
     "crates/omni-android/src/jni/cookies.rs",
     "            .filter(|(_, c)| !c.secure || url.secure)",
     "",
     ["cargo", "test", "-p", "omni-android", "--release", "--lib", "--no-fail-fast", "cookie"]),
    # Code invalidation only for ranges with an executable page (2026-09-24): the allocator's
    # data traffic overflowed sleeping threads' queues and each overflow discarded a whole cache.
    ("inval-A1", "A", "an executable range one thread unmaps is no longer invalidated anywhere",
     "crates/omni-android/src/bionic/guestmem.rs",
     "    space.any_executable(at, len)",
     "    let _ = (space, at, len); false",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "a_range_one_guest_thread_unmaps"]),
    ("inval-A2", "A", "the executable test reads a data page as executable",
     "crates/omni-mem/src/space.rs",
     "            if RegionInfo::from_entry(start, entry).protection == Protection::ReadExecute {",
     "            if RegionInfo::from_entry(start, entry).protection != Protection::None {",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "a_range_one_guest_thread_unmaps"]),
    ("inval-B1", "B", "a data munmap is broadcast to every thread again",
     "crates/omni-android/src/bionic/guestmem.rs",
     """    if executable_within(space, at, len) {
        invalidate(c, at, len)?;
    }
    match space.unmap(at, len) {""",
     """    invalidate(c, at, len)?;
    match space.unmap(at, len) {""",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "a_range_one_guest_thread_unmaps"]),
    ("inval-B2", "B", "a data madvise(DONTNEED) is broadcast to every thread again",
     "crates/omni-android/src/bionic/guestmem.rs",
     """        if executable_within(space, at, len) {
            c.invalidate_code(at, len)?;
        }""",
     """        c.invalidate_code(at, len)?;""",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "a_range_one_guest_thread_unmaps"]),
    ("inval-B3", "B", "a data mprotect is broadcast to every thread again",
     "crates/omni-android/src/bionic/guestmem.rs",
     """    if executable_within(space, at, len) {
        invalidate(c, at, len)?;
    }
    match space.protect(at, len, protection) {""",
     """    invalidate(c, at, len)?;
    match space.protect(at, len, protection) {""",
     ["cargo", "test", "-p", "omni-android", "--release", "--test", "bionic", "--no-fail-fast", "a_range_one_guest_thread_unmaps"]),
]


def read_exactly(path):
    """Read a file without touching its line endings.

    `open(path)` in text mode is universal-newlines: it turns CRLF into LF on the way in, and on
    Windows turns LF back into CRLF on the way out. So a mutation applied to an LF file used to
    restore it as CRLF -- every line of it reported as changed by `git diff`, and the "always
    restored" promise in this module's docstring quietly untrue. `newline=""` on both halves makes
    the round trip exact, which is the only version of that promise worth making.
    """
    with open(path, "r", encoding="utf-8", newline="") as handle:
        return handle.read()


def write_exactly(path, text):
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(text)


def as_written(pattern, text):
    """Re-express a table pattern in the line ending the target file actually uses.

    The two halves of the restore fix have to be done together, and doing only the first is a trap I
    walked straight into. Reading with `newline=""` stops the harness rewriting a file's line
    endings -- but it also means a CRLF file now contains CRLF, while every `old`/`new` string in the
    table above is written with LF, because it lives in a Python source file. Six multi-line patterns
    silently stopped matching and were reported as MISS.

    They were reported, though, which is the only reason this was caught: a MISS is never a pass.
    That is worth more than the bug cost.
    """
    crlf = '\r\n' in text
    normalised = pattern.replace('\r\n', '\n')
    return normalised.replace('\n', '\r\n') if crlf else normalised


def run(command):
    started = time.time()
    proc = subprocess.run(command, capture_output=True, text=True, encoding="utf-8",
                          errors="replace")
    out = (proc.stdout or "") + (proc.stderr or "")
    return proc.returncode, out, time.time() - started


def failing_tests(output):
    names = []
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("test ") and line.endswith(" ... FAILED"):
            names.append(line[len("test "):-len(" ... FAILED")])
    return names


def aborted_test(output):
    """The test that was running when a test binary died, from a `--test-threads=1` run.

    Some mutations are caught by a **crash** rather than by an assertion -- removing a bounds check
    that guest code reaches, for instance, turns a checked refusal into a wild dereference. libtest
    never prints a result line for those, so the harness could only say "the suite failed without
    naming a test", which is caught but useless: it does not say *what* noticed, and the whole value
    of this table is the mapping from a fix to the test that pins it.

    With `--test-threads=1` libtest prints `test <name> ... ` **before** running each one and
    completes the line afterwards, so an *orphan* -- a start with no matching completion -- is a test
    that died. Matching by name matters: `--no-fail-fast` means later binaries keep running and
    completing their own tests, and a naive "last dangling line" is cleared by the first of them.
    """
    orphans = []
    pending = None
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped.startswith("test "):
            continue
        body = stripped[len("test "):]
        if body.endswith("..."):
            # A start. Anything still pending never completed.
            if pending is not None:
                orphans.append(pending)
            pending = body[: -len("...")].strip()
        elif " ... " in body:
            name = body.split(" ... ", 1)[0].strip()
            if pending == name:
                pending = None
            elif pending is not None:
                orphans.append(pending)
                pending = None
    if pending is not None:
        orphans.append(pending)
    return orphans[0] if orphans else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default=None, help="run mutations whose id starts with this")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()

    # **Row ids must be unique, and nothing used to check.** Six rows added for M3 task 3 phase 3a
    # were filed under `plat-*`, and four of them collided with the fault handler's existing
    # `plat-A1`..`plat-A4`. Nothing complained: a full run still touched every row, so the totals
    # were right, but `--only plat-A1` selected two different mutations and a report naming a row
    # id no longer identified one. That is the count-cannot-see-a-substitution failure this project
    # has already been bitten by, in the harness that exists to catch it.
    seen = {}
    collisions = []
    for row in MUTATIONS:
        if row[0] in seen:
            collisions.append(f"  {row[0]}: {seen[row[0]]}  AND  {row[2]}")
        seen[row[0]] = row[2]
    if collisions:
        print(f"{len(collisions)} duplicate mutation id(s). Nothing was run.")
        print(chr(10).join(collisions))
        return 2

    selected = [m for m in MUTATIONS if args.only is None or m[0].startswith(args.only)]
    if args.list:
        for mid, direction, description, path, _, _, _ in selected:
            print(f"{mid:<9} {direction}  {description}  [{path}]")
        return 0

    # Pre-flight: every selected pattern must match its file exactly once *before* anything is
    # mutated or any `cargo` is run.
    #
    # This exists because the Task 1 report claimed it existed when it did not -- the check was
    # per-row, inside the loop, so a stale pattern surfaced as a MISS forty minutes into a run,
    # mixed in with real results. Per-row checking is still there and still needed (a row can go
    # stale between this pass and its turn); this is the cheap pass that says so in one second.
    stale = []
    for mid, _, _, path, old, _, _ in selected:
        text = read_exactly(path)
        found = text.count(as_written(old, text))
        if found != 1:
            stale.append(f"  {mid}: pattern matches {found} times in {path}")
    if stale:
        print(f"pre-flight failed: {len(stale)} of {len(selected)} patterns do not match "
              f"exactly once. Nothing was mutated and nothing was run.")
        print(chr(10).join(stale))
        return 2
    print(f"pre-flight: {len(selected)}/{len(selected)} patterns match exactly once")

    # Pre-flight 2: **every command must PASS on the unmutated tree.**
    #
    # This exists because it did not, and the hole is the worst one a mutation harness can have: a
    # command that already fails reports every row that uses it as `caught`, because "the suite
    # failed" is the whole of what `caught` means here. Eight `time-*` rows were reported 8/8
    # caught that way -- `gmtime(i64::MIN)` panicked with an arithmetic overflow in a **debug**
    # build, which is what this harness runs, while the whole-workspace suite runs `--release` and
    # wrapped silently instead. The defect was real and is fixed; the eight "caught"s were worth
    # nothing until it was.
    #
    # One run per distinct command rather than per row, so a full table costs a handful of extra
    # runs rather than two hundred.
    commands = []
    for row in selected:
        if row[6] not in commands:
            commands.append(row[6])
    for command in commands:
        code, output, seconds = run(command)
        if code != 0:
            print(f"pre-flight failed: `{' '.join(command)}` does not pass on the unmutated tree "
                  f"({seconds}s). Nothing was mutated. Every row using this command would have "
                  f"been reported `caught` whatever its mutation did.")
            for name in failing_tests(output):
                print(f"  {name}")
            return 2
    print(f"pre-flight: {len(commands)}/{len(commands)} commands pass on the unmutated tree")

    print(f"{len(selected)} mutations\n")
    results = []
    for mid, direction, description, path, old, new, command in selected:
        original = read_exactly(path)
        old = as_written(old, original)
        new = as_written(new, original)
        if old not in original:
            print(f"{mid:<9} MISS  pattern not found in {path}")
            results.append((mid, direction, description, "MISS", "pattern not found"))
            continue
        if original.count(old) != 1:
            print(f"{mid:<9} MISS  pattern is not unique in {path}")
            results.append((mid, direction, description, "MISS", "pattern not unique"))
            continue
        try:
            write_exactly(path, original.replace(old, new))
            code, output, seconds = run(command)
            caught = failing_tests(output)
            if code == 0:
                status, detail = "NOT CAUGHT", "every test still passed"
            # Parenthesised. Without them this read as
            # `(not caught and "error[" in output) or ("could not compile" in output)`, so any run
            # whose output happened to contain "could not compile" -- including one where a mutation
            # was genuinely caught by a failing test -- was filed as MISS. A harness that
            # misclassifies its own results is worse than no harness.
            elif not caught and ("error[" in output or "could not compile" in output):
                # **Retried once, and the retry is the point.** MEASURED: one 298-row run produced
                # two of these and BOTH compiled fine afterwards -- `mem-A18` logged
                # "did not compile (2s)" where its real build and suite take 23s, and `varargs-A8`
                # the same. Two seconds is not a compile; it is a cargo lock or a filesystem race.
                #
                # A transient build failure is indistinguishable from a genuinely non-compiling
                # mutation at this point, and filing it as MISS sends somebody to investigate a row
                # that is fine -- the mirror of the misclassification the comment above records. A
                # mutation that truly does not compile fails twice; a race does not.
                code, output, retry_seconds = run(command)
                seconds += retry_seconds
                caught = failing_tests(output)
                if code == 0:
                    status, detail = "NOT CAUGHT", "every test still passed (build retried)"
                elif not caught and ("error[" in output or "could not compile" in output):
                    status, detail = "MISS", "did not compile, twice"
                elif caught:
                    status = "caught"
                    detail = f"{len(caught)} test(s), after a build retry: " + ", ".join(caught[:3])
                    if len(caught) > 3:
                        detail += f", +{len(caught) - 3} more"
                else:
                    status, detail = "caught", "the suite failed after a build retry"
            elif caught:
                status = "caught"
                detail = f"{len(caught)} test(s): " + ", ".join(caught[:3])
                if len(caught) > 3:
                    detail += f", +{len(caught) - 3} more"
            else:
                # Caught by a crash. Re-run serially to find out which test was on the stack --
                # see `aborted_test`.
                serial_code, serial_output, serial_seconds = run(
                    list(command) + ["--", "--test-threads=1"]
                )
                seconds += serial_seconds
                named = aborted_test(serial_output) if serial_code != 0 else None
                if named:
                    status = "caught"
                    detail = f"1 test(s), by abort: {named}"
                else:
                    status = "caught"
                    detail = "the suite failed without naming a test, in parallel and serially"
            print(f"{mid:<9} {status:<10} {description} -> {detail} ({seconds:.0f}s)")
            results.append((mid, direction, description, status, detail))
        finally:
            write_exactly(path, original)
            if read_exactly(path) != original:
                # The restore is the one thing this harness must never get wrong: a file left
                # mutated silently poisons every later row and, worse, the repository.
                print(f"{mid:<9} FATAL restoring {path} did not reproduce the original")
                return 2

    print()
    caught = sum(1 for r in results if r[3] == "caught")
    print(f"{caught}/{len(results)} caught")
    for mid, direction, description, status, detail in results:
        if status != "caught":
            print(f"  {status}: {mid} {direction} {description} ({detail})")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
