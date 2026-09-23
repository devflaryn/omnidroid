# The macOS port

Branch `port-macos`, from `base-0923-night` (`ce10eb8`). Host: Apple M1, 16 GB, macOS 26.5.2
(25F84), Apple clang 16, rustc 1.97.1, CMake 4.4.3, Ninja 1.13.2, MoltenVK 1.4.2, Vulkan loader
1.4.357. **Everything below marked MEASURED was run on that machine**; every figure carries its n
and method. Nothing here is claimed for Intel Macs.

> This document is being written as the port proceeds. Sections marked *(in progress)* are not yet
> verified; the status table is the honest state.

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

# 4. Build and run the platform suites.
cd ~/Desktop/omnidroid
cargo test -p omni-platform -p omni-mem --release
```

The dynarmic build needs nothing else: its Boost subset and every external are vendored, and on a
non-Windows target the build asks CMake for the vendored copies explicitly
(`DYNARMIC_USE_BUNDLED_EXTERNALS=ON`), because with Homebrew present CMake otherwise finds
`/opt/homebrew/lib/cmake/fmt` and links a library the pin never named (MEASURED).

## Status *(in progress)*

| Seam / piece | State on macOS | Evidence |
|---|---|---|
| `vm` | implemented | `vm_macos` 17, `vm_footprint_macos` 1, `omni-mem` space 41 + arena 9; `mac-plat-A*/B*` 11/11 |
| `fs` (`pread`, `pwrite`, `fallocate`, `statvfs`) | implemented | lib tests; `mac-plat-C1..C4` |
| `process` | implemented | lib tests; `mac-plat-C5..C8` |
| `net` | implemented | `net_loopback` 27, `net_macos` 4, `net_seam` 11; `mac-plat-D*` 9/9 |
| `fault` | implemented | `fault_macos` 10, `fault_teardown_race` 2; `mac-fault-*` 9/9 |
| `clock` | portable `std`; timer resolution is a no-op here (see "Timers") | |
| `window`, `audio`, gfx surface | *(in progress, workstream window)* | |
| dynarmic arm64, `omni-cpu` | *(in progress, workstream cpu)* | |
| `webview` | **not ported** (structural `Unsupported`): WKWebView is the macOS equivalent | |

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

## A native CPU backend: first numbers *(in progress)*

The guest ISA is this host's, so a backend that runs guest code natively under
Hypervisor.framework (guest at EL0 in a VM whose stage-2 maps host memory at IPA == VA, keeping
D4's identity mapping; `svc` and thunk calls trapping to the host) is the obvious candidate to
replace translation. Two facts decide its shape, MEASURED with a C probe (`hv_vm_create`,
`hv_vcpu_run` over a guest `hvc #0; b .-4` loop at EL1, binary ad-hoc signed with
`com.apple.security.hypervisor`):

| Quantity | Value |
|---|---|
| one VM exit + resume (`hvc` -> host -> `hv_vcpu_run`) | **708 ns** (best of 3 rounds, n = 200,000 each; 836, 729, 708) |
| vCPUs per VM (`hv_vm_get_max_vcpu_count`) | **64** |

Against D17's in-loop import dispatch (26.7-31.0 ns on the x64 host), an import that became a VM
exit would cost ~25x more, and Windows measured the engine crossing the import boundary about
1.3 million times a second (VERIFICATION entry 15, a startup phase on Windows) -- which at 708 ns is ~0.9 s of exits per second
of guest time. And the engine runs up to 256 guest threads (`MAX_GUEST_THREADS`) against 64 vCPUs.
So a hypervisor backend is only a win if the hot imports stop being exits (served in-guest) and
guest threads are multiplexed onto vCPUs; whether the compute it buys back outweighs that is the
number still to be measured, in the world, once parity holds.

## Merge notes

Shared files this port edits, each minimal and additive, none changing Windows behaviour:

| File | Edit |
|---|---|
| `crates/dynarmic-sys/build.rs` | C compiler for CMake off MSVC; `DYNARMIC_USE_BUNDLED_EXTERNALS=ON` off Windows; no Zydis/Zycore link off x86-64 |
| `.gitignore` | dynarmic's mig output under `vendor/dynarmic/src/dynarmic/backend/*/mig/` |
| `tools/mutate.py` | one line appending `tools/mutate_mac` rows |
| `crates/omni-platform/src/{vm,fs,net,process}/mod.rs` | `#[cfg_attr(target_os = "macos", allow(dead_code))]` on `mod unix` |
| `crates/omni-platform/src/fault/mod.rs` | a `macos` backend arm; `unsupported` for neither Windows nor macOS |
| `crates/omni-platform/src/process/error.rs` | `ProcessError::Errno` variant (errno-reporting calls) |
| `crates/omni-platform/src/process/mod.rs` | seam tests that said "only Windows answers" include macOS |
| `crates/omni-platform/tests/vm_seam.rs` | the structural-backend test excludes macOS |
| `crates/omni-platform/tests/net_loopback.rs` | runs on macOS; the second `SO_ERROR` read asserted per host |
| `crates/omni-platform/tests/fault_teardown_race.rs` | runs on macOS; waits until the primary is entered before the teardown, on macOS only |
| `crates/omni-mem/tests/{space,arena,config,windows_only}.rs` | run on macOS; 4 KiB / 64 KiB-base / Windows-code expectations ask the host |

**Linux port:** `fs/unix.rs`, `net/unix.rs`, `process/unix.rs`, `vm/unix.rs` are not used on macOS
any more (macOS has its own `macos.rs` bodies); the only change near them is the `allow(dead_code)`
attribute on their `mod` lines. `process/unix.rs`'s note that macOS has no `sched_getcpu` is
outdated: `pthread_cpu_number_np` (macOS 11+) is what `process/macos.rs` uses.
