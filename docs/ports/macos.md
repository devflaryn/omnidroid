# The macOS port

Branch `port-macos`, from `base-0923-night` (`ce10eb8`). Host: Apple M1, 16 GB, macOS 26.5.2
(25F84), Apple clang 16, rustc 1.97.1, CMake 4.4.3, Ninja 1.13.2, MoltenVK 1.4.2, Vulkan loader
1.4.357. **Everything below marked MEASURED was run on that machine**; every figure carries its n
and method. Nothing here is claimed for Intel Macs.


## Build from a fresh Mac

```sh
# 1. Command-line tools (clang, mig, the SDK) and Rust.
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. Homebrew (no sudo is needed after its own install), then the build and Vulkan pieces.
brew install cmake ninja molten-vk vulkan-loader
#    optional, for `vulkaninfo`:  brew install vulkan-tools

# 3. The APK beside the source, as on every host.
cp Roblox-2.738.1397.apk ~/Desktop/omnidroid/

# 4. Build and run everything (the first build compiles dynarmic with CMake: a few minutes).
cd ~/Desktop/omnidroid
cargo test --workspace --release --no-fail-fast

# 5. The gate: a window, MoltenVK, the engine's landing screen. The window-server capture test also
#    needs Screen Recording permission for the terminal (System Settings > Privacy & Security).
OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1 OMNI_KEYBOARD_MOUSE=1 cargo test -p omni-android \
  --release --test gameactivity -- --nocapture --test-threads=1 \
  initialize_native_code_returns_a_native_code_and_the_game_thread_starts

# 6. Play: the same run with a long session, the storage kept (tools/play.sh --help).
tools/play.sh

# Optional: memory from outside the process, while a run is going.
python3 tools/footprint_mac.py --match '[d]eps/gameactivity'

# Optional: the native backend (D34, not the default). Its test binaries must be signed with the
# hypervisor entitlement, which tools/hvf_run.sh does as the cargo runner.
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh \
  cargo test -p omni-cpu --release --features native-hvf --test native -- --test-threads=1
```

The APK must be a regular file or a hard link, not a symbolic link: the gate hard-links it into the
guest's root, and the guest filesystem refuses a link to a symlink (MEASURED in two worktrees).

The dynarmic build needs nothing else: its Boost subset and every external are vendored, and on a
non-Windows target the build asks CMake for the vendored copies explicitly
(`DYNARMIC_USE_BUNDLED_EXTERNALS=ON`), because with Homebrew present CMake otherwise finds
`/opt/homebrew/lib/cmake/fmt` and links a library the pin never named (MEASURED).

## Status

| Seam / piece | State on macOS | Evidence |
|---|---|---|
| `vm` | implemented | `vm_macos` 17, `vm_footprint_macos` 1, `omni-mem` space 41 + arena 9; `mac-plat-A*/B*` 11/11 |
| `fs` (`pread`, `pwrite`, `fallocate`, `statvfs`) | implemented | lib tests; `mac-plat-C1..C4` |
| `process`, threads | implemented: entropy, current CPU (`pthread_cpu_number_np`), thread priority as QoS classes, CPU time | lib tests; `mac-plat-C5..C8` |
| `net` | implemented | `net_loopback` 27, `net_macos` 4, `net_seam` 11; `mac-plat-D*` 9/9 |
| `fault` | implemented | `fault_macos` 10, `fault_teardown_race` 2; `mac-fault-*` 9/9 |
| `clock` | portable `std`; timer resolution is a no-op here (see "Timers") | |
| `window` (AppKit), `audio` (Core Audio), gfx surface (MoltenVK) | implemented | see `docs/ports/macos-window.md`; `mac-win-` 22/22, `mac-gfx-` 9/9 |
| dynarmic arm64, `omni-cpu` | parity: carried patches 0002-0009 and 0014 (the store-exclusive fault, below) | see `docs/ports/macos-cpu.md`; `mac-cpu-` 25/25 + `mac-cpu-E1` |
| native backend (Hypervisor.framework) | built, measured, **not adopted** (D34) | `macos-hvf.md`; `mac-hvf-` 26/26 |
| **the gate** | **passes**, landing screen reached | below |
| memory | boot peak **862-1,122 MiB**, steady **811-841 MiB** at the landing screen; ten instances on 8 GB **not shown** (4 of 4 on this 16 GB Mac) | below; `macos-memory.md`; `mac-mem-` 10/10 |
| `webview` | **not ported** (structural `Unsupported`): WKWebView is the macOS equivalent | |
| ELF loader on 16 KiB pages | relro sealed as bionic seals it; `libzstd-jni` (`p_align` 0x1000) refused by name | `loader_m1` 13, `loader_hostile` 25; `mac-elf-A1` 1/1 |
| **the whole workspace** | `cargo test --workspace --release --no-fail-fast`: 167 suites, **2,069 passed, 1 failed** (the headless gate's `eglGetDisplay`, as on Windows), 90 ignored | 2026-09-24, `port-macos` at `0e7d5c0` (every workstream merged) |

## The gate on macOS

`OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1 OMNI_KEYBOARD_MOUSE=1 cargo test -p omni-android
--release --test gameactivity -- --nocapture --test-threads=1
initialize_native_code_returns_a_native_code_and_the_game_thread_starts` **passes** (exit 0; 5 runs
on `port-macos`, 50-110 s each). MEASURED in those runs:

* the engine's Vulkan device is MoltenVK's **Apple M1** (`PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU`, not
  refused as emulated), a 1280x720 swapchain with 3 images, and the engine reaches
  `APP_READY(Landing)` -- the real landing screen, captured from the window server below;
* the network is real: client settings are fetched (`Flag::areFlagsLoaded` 1), `apis.roblox.com`
  answers 401/404 to an unsigned-in client, as it does to a device; nothing was signed in;
* **no guest thread is killed** (`M5 teardown: 0 guest thread(s) still running, failures []`);
* at the landing screen the engine presents **~59 fps** (291-299 presents per 5 s) while it is
  animating, which is the engine's own 60 fps cap, not this host's limit. Once the landing has
  settled it draws **~1 fps** (+1 to +5 presents per 5 s) with no input -- the same "idle 1 fps"
  HANDOFF records on Windows (frontier item 3), not a difference this port makes.

Rerun after the relro fix (`47b0409`, which changes what the real `libroblox.so` load seals): exit
0, Apple M1, `APP_READY(Landing)` at +17.9 s, 0 guest threads killed; `phys_footprint` peak 2,135
MiB, 1,957-2,005 MiB at the landing (n = 1 run, 1 s samples).

![The engine's landing screen on macOS](macos-landing.png)

Headless (`OMNI_GFX_WINDOW_TESTS` unset) the engine falls back to GLES and a thread dies on unbound
`eglGetDisplay`: the same failure HANDOFF records for Windows ("still open", item 7).

### Memory in the gate (MEASURED, `tools/footprint_mac.py`, 1 s samples of `phys_footprint`)

| | before patch 0009 (n = 1) | + patch 0009 (`ef65a2d`) | + patches 0010-0013, load buffer released (`mac-mem`) |
|---|---|---|---|
| boot peak (`ri_lifetime_max_phys_footprint`) | 3,260 MiB | 2,492-2,878 MiB (n = 3) | **1,045-1,122 MiB** (n = 4); **862 MiB** on the merged tree (n = 1) |
| steady at the landing screen (+60 s) | ~3,210 MiB | 2,420-2,698 MiB, still climbing (n = 3) | **811-841 MiB**, flat (n = 4); **824-826 MiB** on the merged tree (n = 1) |

Against the owner's targets: **boot ~4 GB -- met** (under 1.2 GiB); **steady ~800 MB -- met to
within ~5%**. Nothing is reserved with backing: the guest space is a 16 GiB `PROT_NONE` reservation
charged only for touched pages, freed guest memory is decommitted (a fresh `MAP_FIXED` mapping, which
is how macOS gives pages back and reads zero afterwards), and read-only file pages -- the libraries,
the APK's assets -- are the file cache's, shared, and charge nothing (the VM table below). Swap was
not moved by any run.

What the per-instance footprint is now, by size (`vmmap`, +60 s): the guest's own memory ~325 MiB
(the engine's; the floor for an instance), dynarmic code caches 169-184 MiB (one translation per guest
thread), host heap ~135 MiB (unattributed), graphics ~85 MiB, dynarmic bookkeeping ~100 MiB (it was
1.19 GiB: 2,099 bytes per translated block and never cleared -- root causes and patches in
[`macos-memory.md`](macos-memory.md)).

**Ten instances on 8 GB: not shown.** Measured on this 16 GB Mac (`footprint_mac.py --launch`, 90 s
apart, other apps open): **4 of 4** instances reached the landing screen and exited 0, 750-860 MiB
each, 3,171 MiB together; free memory went 62% -> 35%, macOS compressed ~4 GB (other processes'
pages too), swap never moved. Ten at ~800 MiB is the whole of an 8 GB machine, so it depends on
compression that was not exercised here; CPU was the tighter limit (with 3-4 instances booting, one
settings fetch took 55 s). In two earlier multi-instance runs (before 0013) an instance failed on
the network (`SslConnectFail`, refused DNS) within a second of another starting -- not investigated.
The deeper cut, one translation cache shared by a process's threads, is scoped in `macos-memory.md`
and not built.

## Virtual memory (D10 on this host)

MEASURED with a probe and then pinned by `vm_footprint_macos`:

| Operation | `phys_footprint` effect |
|---|---|
| reserve 16 GiB `PROT_NONE` | +0.00 MiB |
| commit (`mprotect` RW) 256 MiB | +0.00 MiB |
| touch every page of it | +256.1 MiB |
| decommit (fresh `MAP_FIXED` mapping) | -256.1 MiB, re-committed pages read 0 |
| read every page of a 64 MiB read-only file view | +0.00 MiB (clean file pages are the file cache's, shared) |
| `madvise(MADV_FREE_REUSABLE)` (not used) | footprint drops, **contents kept** -- cannot implement zero-on-recommit |
| `madvise(MADV_DONTNEED)` (not used) | no measurable effect |

* **The page is 16 KiB**, and so is the allocation granularity. The guest is told the same through
  `AT_PAGESZ`/`_SC_PAGESIZE` (they already read `vm::page_size()`), which is the host's truth and
  what an Android 15 16 KiB-page device says; `libroblox.so`'s `PT_LOAD`s are 16 KiB-aligned.
* **There is no commit charge on macOS.** Memory is backed on first touch. `process_commit_charge`
  answers `phys_footprint` (dirty private + compressed + swapped), which is what the kernel's
  memory-pressure policy acts on and the scarce resource in D10's sense -- but it moves on touch,
  not on commit, and is documented so. `omni-mem/tests/commit_charge.rs` stays Windows-only.
* **A file cannot be mapped `PROT_EXEC`** (`mmap` answers `EPERM` for an ordinary file) but a
  read-only view can be `mprotect`ed to r-x. `map_file(ReadExecute)` is those two calls, and the page
  really is r-x (checked with `mach_vm_region`). Guest code is never run natively by dynarmic.
* The seam's Windows-shaped refusals (exact-size placeholder, view base/length, release extent, the
  non-executable-section cap) are made from a registry the backend keeps, because `mmap(MAP_FIXED)`
  would silently replace anything.
* **`__PAGEZERO`** is 4 GiB on arm64 executables, so guest addresses below 4 GiB fault, the way
  Android's `mmap_min_addr` makes low addresses fault. No guest mapping is placed there: every guest
  address comes from an `omni-mem` reservation, and nothing in the workspace asks for a fixed low
  address (checked by search).

## The ELF loader on 16 KiB pages

* **Relro is sealed as bionic seals it**: `[page_start(p_vaddr), page_end(p_vaddr + p_memsz))`, the
  end rounded **up** (`_phdr_table_set_gnu_relro_prot`, AOSP `linker_phdr.cpp`). The loader rounded
  it down, which is identical wherever relro ends on a page boundary -- every library of this APK at
  4 KiB -- and is not at 16 KiB: `libroblox.so`'s relro ends at `0x67d3000`, inside the 16 KiB page
  holding `.got`/`.got.plt`, which stayed writable here. `.data` starts on the next 16 KiB page
  (`0x67d67c0`), so sealing the whole page takes nothing writable away (`loader_m1` asserts both).
  Row `mac-elf-A1` (round down again) is caught by three `loader_m1` tests.
* **A library aligned below the host page is refused by name** (`AlignBelowPageSize`), as bionic
  refuses it on a 16 KiB device. Of the APK's eleven, that is `libzstd-jni-1.5.7-6.so` (`p_align`
  0x1000) and nothing else. The gate loads only `libroblox.so`, on every host (`dlopen` of any other
  file is refused, `bionic/dl.rs`), so the run is unaffected; `libzstd-jni` is the Java side's zstd
  binding, and the library into which, per `fs/path.rs`, a third party added a Luau executor. Loading it would need a copying
  loader, as Android 16's page-size compatibility mode has; not built, because nothing here needs it.
* Nothing in the APK is directly mappable from the zip at 16 KiB (the one PNG that is, at 4 KiB, is
  4 KiB-aligned); libraries already go through the extraction cache on every host (D11).

## Guest faults (D4, D10)

Thread-level Mach exception ports on every thread; handlers run on the faulting thread through a
trampoline; a decline falls through to the task port (dynarmic's) and then to the signal. The full
argument is in `crates/omni-platform/src/fault/macos.rs`. MEASURED: **43.5 us per resolved fault**
including one decommit (n = 2,000), against 2.05 us for Windows' VEH (D10). The pager is not the
commit hot path (D10), so this is a cost on first touch of lazily-committed guest memory.

## Network

MEASURED differences from Windows and Linux, each handled in `net/macos.rs`:

* a refused non-blocking connect polls as `POLLHUP` alone (Linux: `POLLIN|POLLOUT|POLLERR|POLLHUP`),
  so a hung-up socket is reported readable/writable when asked, and `SO_ERROR` says why;
* `SO_ERROR` is cleared by the read (Winsock does not clear it; asserted per host);
* `SIGPIPE`: `SO_NOSIGPIPE` on every created and accepted socket (tested in a child that restores
  `SIG_DFL`, because Rust binaries ignore `SIGPIPE`);
* path-MTU discovery is one don't-fragment bit (`IP_DONTFRAG`); `Probe` behaves as `Do` here and
  the difference is named, not hidden;
* `getifaddrs` lists one link-local address on the two AWDL interfaces; reported once.

## Timers

MEASURED (n = 300-1000 each, C probe): a 1 ms `nanosleep` takes **1.49 ms** on average (timer
coalescing leeway), `QOS_CLASS_UTILITY` stretches it to **7.0 ms**, a `THREAD_TIME_CONSTRAINT`
thread gets 1.01 ms, `pthread_cond_timedwait` 1.51 ms, and an `EVFILT_TIMER` with `NOTE_CRITICAL`
1.03 ms. `NSActivityLatencyCritical` does **not** change it for a non-app process (1.50 ms). Windows'
answer to the same problem is `TimerResolution::raise` (1 ms); this host needs a different one --
recorded for the performance work, not yet acted on.

## A native CPU backend (Hypervisor.framework): measured, not adopted -- D34

The guest ISA is this host's, so the port built a native backend behind `GuestCpu`
(`omni-cpu`'s `native-hvf`, off by default): the guest at EL0 under Hypervisor.framework, stage 2
at IPA == VA so D4's identity mapping holds, a stage-2 mirror of host protection fed from
`vm/macos.rs`, demand paging through `omni_mem::admit`. It runs the M2 gate on the real
`libroblox.so` and all 3,594 initializers. Everything, with n and method, is in
[`macos-hvf.md`](macos-hvf.md); the decision is **D34** in `docs/DECISIONS.md`. The numbers it turns
on (M1, release):

| | dynarmic | native |
|---|---|---|
| one import crossing | 23.7 ns inline | **1,614 ns** (VM exit + full register save) |
| real engine compute | 1.32 G insn/s | **7.55 G insn/s** |
| 3,594 initializers, cold (n = 8) | 2,417 ms | **795 ms** |
| memory per guest thread | ~33 MB at the landing screen | **79 KiB** |

At the landing screen the working threads cross the import boundary 0.40-0.52 M times a second,
which natively is 0.64-0.84 of a core in exits -- about what the faster code saves there. **Not
adopted**: dynarmic is the backend every gate runs on. D34 lists the conditions (hot imports served
in the guest, under 50k exits/s; no counted budgets for a backend that cannot count; the gate
passing natively; more than 64 threads; the hypervisor entitlement).

**Found on the way: dynarmic arm64 killed the process on a guest store-exclusive to read-only
memory** (Global Constraint 11). The native backend's survey of all 245,117 engine functions reached
it at `libroblox.so + 0x224822c`; delta debugging reduced it to two functions, the outlined
`__aarch64_swp8_rel` swapping through a pointer into sealed `.data.rel.ro`. Patch 0007's inline
store-exclusive registered only its load as a fastmem patch location, so the store's write fault
reached dynarmic's handler unrecorded and it aborted. **Patch 0014** registers the store too; the
function is now a typed write fault and the dynarmic survey runs all 245,117 functions (41 s, n = 1)
without an abort. Tests: `dynarmic-sys/tests/host_fault.rs` (both widths, read-only data of the test
binary) and `omni-cpu/tests/exclusive_store_fault.rs` (the two real functions); row `mac-cpu-E1`.

## Mutation rows (`python3 tools/mutate.py --only mac-`, the merged tree)

**130 of 131 caught** (2026-09-24, `port-macos` after every merge; pre-flight 131/131 patterns match
once, 31/31 commands pass unmutated; the tree was clean afterwards and `vendor/PIN.txt` touched
again). The one row not caught is **`mac-cpu-A15`** (patch 0008 reverted: the memory-abort check
loads the u32 halt word with a 64-bit `LDAR`), and it is not a hole in a test but an equivalent
mutant *on this layout*. The M1 implements FEAT_LSE2, so an unaligned `LDAR` faults only when it
crosses a 16-byte boundary; patches 0010/0011 changed `A64AddressSpace`'s size, which moved
`halt_reason` in `Jit::Impl` to 4 mod 16, where the 64-bit load completes (reading 4 neighbouring
bytes that `TST` masks off). Shown by hand, not assumed: the same mutation with a pad of 8 bytes
before `halt_reason` (the word at 12 mod 16) aborts `host_fault`; with 0, 4 or 12 bytes it passes.
0008 stays: the load is wrong at any offset and fatal at one the layout can move to.

## Still open, with the consequence

* **Ten instances on 8 GB is not demonstrated** (four on this 16 GB Mac, 3.2 GB together). The next
  cut is one translation cache per process instead of per guest thread (`macos-memory.md`), and
  omni-android forwarding only executable ranges to the jits (it sends every guest `munmap`/`mprotect`
  to every thread, which 0012 made cheap in memory but not in CPU).
* **120 fps is not measured.** The landing screen presents at the engine's own 60 fps cap while it
  animates and ~1 fps once settled (as on Windows); a game world needs a signed-in session, which this
  port does not create. The native backend's compute is 5.7x dynarmic's, but its crossings cost more
  than that saves at the landing screen (D34).
* **Timer leeway**: a 1 ms sleep takes 1.49 ms here ("Timers"); nothing acts on it yet.
* **Headless `eglGetDisplay`** kills a guest thread when no window is asked for -- the same on
  Windows (HANDOFF "still open" 7).
* **Window seam** (`macos-window.md`): captured pointer motion is accelerated (the raw-delta reader,
  `IOHIDManager`, is not built); the physical wheel's sign under natural scrolling is documented,
  not measured; the portability subset has no validation-layer detector on this host.
* **`webview`** is not ported (WKWebView would be its body); anything that opens a web view gets the
  seam's typed `Unsupported` refusal.
* **`libzstd-jni`** (`p_align` 0x1000) cannot load on a 16 KiB host without a copying loader; nothing
  in the gate loads it.

## Merge notes

Every edit this branch makes to a file that also compiles on Windows or Linux, in one place. The
workstream notes (`macos-cpu.md`, `macos-window.md`, `macos-memory.md`, `macos-hvf.md`) carry the
reasoning; this is the list a merge needs. **Windows behaviour is unchanged** except where a row
says otherwise, and each such row says what changes and why.

### Changes a Windows build sees

| File | Edit | Windows |
|---|---|---|
| `crates/omni-elf/src/loader/mod.rs` | `relro_region` rounds relro's **end up** (bionic's `page_end`), not down | the same pages for this APK: every library's relro ends 4 KiB-aligned (checked, all 11). A library whose relro ends mid-page now gets that page sealed, as bionic seals it |
| `crates/omni-gfx/src/vulkan.rs`, `host.rs` | loader through `portability::load_entry`; the instance retry with portability enumeration fires only after `VK_ERROR_INCOMPATIBLE_DRIVER`; one extra `vkEnumerateDeviceExtensionProperties` before `vkCreateDevice`; a zero-size `Resized` skips the swapchain rebuild | same loader, same instance request; the extra enumeration is a query |
| `crates/dynarmic-sys/build.rs` | a C compiler for CMake off MSVC; `DYNARMIC_USE_BUNDLED_EXTERNALS=ON` off Windows; Zydis/Zycore linked only on x86-64 | MSVC path untouched |
| `crates/dynarmic-sys/shim/od_dynarmic.{h,cpp}`, `src/lib.rs` | `OD_CODE_CACHE_*` constants (0 and 1 as before), the Apple-arm64 W^X branch, `od_jit_last_svc_return_address` under `__aarch64__` only; `OD_FIXED_PER_JIT_BYTES` per architecture | x86-64 values and ABI version unchanged; no struct layout changed |
| `crates/dynarmic-sys/vendor/` + `patches/0002-0009` | eight fixes to dynarmic's **arm64** backend | not compiled for x86-64 targets (one `#include` of an x64 header from arm64 code) |
| `crates/omni-cpu/{Cargo.toml,src/lib.rs,src/dynarmic/mod.rs}` | `dynarmic` for `any(x86_64, aarch64)`; `mxcsr` is x86-64-only with an FPCR twin re-exported under the same name on aarch64 | body unchanged on x86-64 |
| `crates/omni-platform/src/window/{mod,error}.rs`, `audio/{mod,error}.rs` | `RawWindow::AppKit`, `vulkan_loader_candidates()` (empty off macOS), additive error variants; `mod unix` gated `not(macos)` | additive; nothing matches these enums exhaustively (checked) |
| `crates/omni-platform/src/fault/mod.rs` | a `macos` backend arm (`all(macos, aarch64)`) | Windows arm unchanged |
| `crates/omni-platform/src/process/{mod,error}.rs` | `ProcessError::Errno`; seam tests that said "only Windows answers" include macOS | additive |
| `crates/omni-platform/src/{vm,fs,net}/mod.rs` | `#[cfg_attr(target_os = "macos", allow(dead_code))]` on `mod unix` | none |
| `crates/omni-platform/Cargo.toml` | `[target.'cfg(target_os = "macos")'.dependencies]` (objc2 family, named features) and dev-dependencies | none |
| `crates/omni-gfx/src/{lib,claim}.rs`, `portability.rs` (new) | `mod portability`; `WindowKey::appkit` | additive |
| `.gitignore`, `tools/mutate.py` | mig output dirs; one line appending `tools/mutate_mac` rows | none |
| `crates/omni-android/src/bionic/{mod,threads}.rs` | a guest thread on a backend that **cannot count** instructions runs unbounded and is stopped through its `HaltHandle` (a registry `stop_guest_threads` requests); exit destructors likewise | none: dynarmic reports `counted_step_limit: true`, so it takes exactly the old path; the registry stays empty |
| `crates/omni-platform/{Cargo.toml,src/lib.rs}`, `src/vm/macos.rs` | a `hypervisor` feature and its `mod` line; `vm/macos.rs` calls `mirrored()` after each map/protect/unmap | off by default; `mirrored` is empty without the feature |
| `crates/omni-cpu/{Cargo.toml,src/lib.rs}`, `crates/omni-android/Cargo.toml` | an aarch64-only optional `omni-platform` dependency and the `native-hvf` feature; `mod native` under it | off by default, arm64 only |
| `crates/omni-android/tests/gameactivity.rs` | `OMNI_CROSSING_RATE=1` prints the import crossings per second | off by default, read-only |
| `docs/DECISIONS.md` | **D34** appended (the native backend: measured, not adopted) | a record |
| `crates/dynarmic-sys/vendor/` patch **0014** | the arm64 inline store-exclusive's store is a fastmem patch location | arm64 only |

### Test files: expectations that now ask the host

Each keeps its Windows value exactly and derives the macOS one from the host (16 KiB pages,
`phys_footprint` instead of commit charge, signals instead of exception codes):

* `omni-elf`: `tests/common/synth.rs` lays the synthetic library out on the host page (identical
  bytes at 4 KiB); `loader_hostile.rs` derives each address from it and expects exactly
  `libzstd-jni` (the one `p_align 0x1000` library) refused with `AlignBelowPageSize` on 16 KiB;
  `loader_m1.rs` host-page relro expectations.
* `omni-apk`: `real_apk.rs` (nothing is directly mappable at 16 KiB), `synthetic_zip.rs` (the aligned
  entry is padded to one host page).
* `omni-android`: all 17 suites `any(x86_64, aarch64)`; `bionic.rs` (buffers straddling a host page,
  `msync`/`ftruncate`/`statm` on this host); `initializers.rs` (the non-pointer-word floor follows
  guest-space placement, which is what it measured); `vulkan_present.rs` (an import is rounded to a
  host page).
* `omni-cpu`: crate cfgs name both architectures; `thunk.rs` MXCSR/FPCR twins; `harness` re-asks for
  a high guest space only when the default is below 64 GiB; `roblox.rs` per-thread floor is one host
  page where the counter charges on touch.
* `omni-mem`: `space`, `arena`, `config`, `windows_only` run on macOS with host-aware values.
* `omni-platform`: `vm_seam`, `window_seam` structural tests exclude macOS; `net_loopback` asserts the
  second `SO_ERROR` read per host; `fault_teardown_race` waits until the primary is entered, on
  macOS only.
* `omni-gfx`: `renderer_live` asserts the layer report equals the loader's own enumeration (this
  host has no implicit layers).
* `dynarmic-sys`: `a64_exec`'s RWX test is x86-64-only (unchanged); `hostile.rs` keeps the 27 x86-64
  cells and has an arm64 table.

### For the Linux port

`fs/unix.rs`, `net/unix.rs`, `process/unix.rs`, `vm/unix.rs`, `window/unix.rs`, `audio/unix.rs` are no
longer compiled into macOS builds' backends (macOS has its own `macos.rs` bodies); the only edits near
them are `cfg` lines on their `mod` statements. `process/unix.rs`'s note that macOS has no
`sched_getcpu` is outdated: `pthread_cpu_number_np` (macOS 11+) is what `process/macos.rs` uses.
